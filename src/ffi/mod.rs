//! Arrow C Data Interface implementation for zero-copy export.
//!
//! Implements ArrowSchema, ArrowArray, and ArrowArrayStream per
//! https://arrow.apache.org/docs/format/CDataInterface.html

use std::ffi::{c_char, c_void, CString};
use std::ptr;
use std::sync::Arc;

use crate::batch::ColBatch;
use crate::column::{
    Column, DynamicChild, DynamicColumn, JsonBody, JsonColumn, NothingColumn, StructuredJson,
    Utf8Column, VariantColumn, VariantGroup, VariantLayout, ARROW_UNION_MAX_CHILDREN,
};
use crate::native::decode::low_cardinality_dict_value_type;
use crate::schema::{geometry_underlying_type, ChType, IntervalKind, Schema};

// ---------------------------------------------------------------------------
// Arrow C Data Interface structs (repr(C) per spec)
// ---------------------------------------------------------------------------

#[repr(C)]
pub struct ArrowSchema {
    format: *const c_char,
    name: *const c_char,
    metadata: *const c_char,
    flags: i64,
    n_children: i64,
    children: *mut *mut ArrowSchema,
    dictionary: *mut ArrowSchema,
    release: Option<unsafe extern "C" fn(*mut ArrowSchema)>,
    private_data: *mut c_void,
}

#[repr(C)]
pub struct ArrowArray {
    length: i64,
    null_count: i64,
    offset: i64,
    n_buffers: i64,
    n_children: i64,
    buffers: *mut *const c_void,
    children: *mut *mut ArrowArray,
    dictionary: *mut ArrowArray,
    release: Option<unsafe extern "C" fn(*mut ArrowArray)>,
    private_data: *mut c_void,
}

#[repr(C)]
pub struct ArrowArrayStream {
    get_schema: Option<unsafe extern "C" fn(*mut ArrowArrayStream, *mut ArrowSchema) -> i32>,
    get_next: Option<unsafe extern "C" fn(*mut ArrowArrayStream, *mut ArrowArray) -> i32>,
    get_last_error: Option<unsafe extern "C" fn(*mut ArrowArrayStream) -> *const c_char>,
    release: Option<unsafe extern "C" fn(*mut ArrowArrayStream)>,
    private_data: *mut c_void,
}

impl ArrowArrayStream {
    /// Invoke the release callback if present, then clear it. Follows the
    /// spec's consumer pattern: the callback runs with `release` still set and
    /// normally clears it itself; clearing afterwards keeps the call
    /// idempotent even for a callback that does not. A stream already released
    /// by a consumer has `release` set to null, making this a no-op.
    ///
    /// # Safety
    ///
    /// `self` must be a stream initialized per the Arrow C Data Interface, for
    /// example by [`export_chunks_to_stream`], or already released or moved
    /// from with `release` cleared to `None`.
    pub unsafe fn release_if_set(&mut self) {
        if let Some(release) = self.release {
            release(self as *mut ArrowArrayStream);
            self.release = None;
        }
    }
}

// ---------------------------------------------------------------------------
// Private data for release callbacks
// ---------------------------------------------------------------------------

struct SchemaPrivateData {
    format: CString,
    name: CString,
    children: Vec<*mut ArrowSchema>,
    /// The dictionary value-type child schema for a `LowCardinality(T)` field,
    /// owned here so it is freed when this field's schema is released. Null for
    /// every non-dictionary field.
    dictionary: *mut ArrowSchema,
}

struct ArrayPrivateData {
    buffers: Vec<*const c_void>,
    children: Vec<*mut ArrowArray>,
    /// Routing buffers synthesized only when a Dynamic column's block-local
    /// u32 child ids must be mapped into Arrow's signed Int8 union codes.
    /// Ordinary columns and already-Arrow-shaped Variant columns leave these
    /// empty and continue to borrow their buffers zero-copy.
    _owned_type_ids: Vec<i8>,
    _owned_offsets: Vec<i32>,
    /// Synthetic all-NULL union routing buffers shared (via Arc) by every
    /// absent-dynamic-path node of one JSON column, so a chunk missing many
    /// result-wide paths allocates them once instead of once per path. `None`
    /// everywhere else.
    _shared_type_ids: Option<Arc<Vec<i8>>>,
    _shared_offsets: Option<Arc<Vec<i32>>>,
    _batch: Arc<ColBatch>,
    /// The dictionary values child array for a `LowCardinality(T)` column, owned
    /// here so it is freed when this column's array is released. Null for every
    /// non-dictionary column.
    dictionary: *mut ArrowArray,
}

struct StreamPrivateData {
    /// Schema for `get_schema`; valid even when there are zero chunks.
    schema: Schema,
    /// Recursive result-wide physical plans. Dynamic discovers its concrete
    /// child types per block, so the stream pre-scans the already-supplied
    /// chunks once and fixes one schema before the first batch is exported.
    plans: Vec<FieldExportPlan>,
    /// Remaining chunks to hand out, one Arrow record batch per `get_next`,
    /// paired with each chunk's index so Dynamic plans can select the routing
    /// table precomputed for that chunk.
    chunks: std::iter::Enumerate<std::vec::IntoIter<Arc<ColBatch>>>,
    /// Set when the result-wide plan cannot be exported at all: a Dynamic child
    /// set beyond [`DYNAMIC_MAX_EXPORT_CHILDREN`] or duplicate Dynamic child
    /// type names. While set, `get_schema` and `get_next` return
    /// [`STREAM_INIT_ERROR`] without writing their `out` struct, and
    /// `get_last_error` returns `error_msg`.
    init_error: bool,
    error_msg: CString,
}

enum FieldExportPlan {
    Plain,
    Array(Box<FieldExportPlan>),
    Tuple(Vec<FieldExportPlan>),
    Map {
        key: Box<FieldExportPlan>,
        value: Box<FieldExportPlan>,
    },
    Variant(Vec<FieldExportPlan>),
    Dynamic(DynamicExportPlan),
    Json(JsonExportPlan),
}

/// Result-wide export plan for one `JSON` field. The body kind (structured vs
/// text) is fixed for a whole query result by a server setting, so the stream
/// planner resolves it once here from the nonempty chunks; a nonempty chunk
/// that disagreed is rejected before the first batch (see
/// [`ExportError::JsonBodyKindMismatch`]). A zero-row chunk carries no JSON
/// state prefix and so votes for neither kind; its array is synthesized to
/// match the planned kind at export time.
enum JsonExportPlan {
    /// `STRING`-mode blocks: every row is one re-serialized JSON document
    /// string, exported exactly like a plain `String` column.
    Text,
    /// Structured blocks export as an Arrow struct. `typed_paths` is the
    /// field's declared typed-path list and `typed` one plan per entry, in the
    /// same order. `dynamic` holds one result-wide `DynamicExportPlan` per
    /// dynamic path name discovered across all chunks, in BTreeMap name order,
    /// so the per-block dynamic path sets unify into one fixed child list. The
    /// trailing `_shared_data` child is plain utf8/binary and needs no plan.
    Structured {
        typed_paths: Vec<(String, ChType)>,
        typed: Vec<FieldExportPlan>,
        dynamic: Vec<(String, DynamicExportPlan)>,
    },
}

struct DynamicExportPlan {
    children: Vec<DynamicPlanChild>,
    /// Per-chunk block-local -> result-wide child index tables, indexed by
    /// chunk. Entry `[chunk][local]` is the planned child fed by that chunk's
    /// local child `local`; `None` for a chunk whose column at this position
    /// was not a Dynamic when the plan was built. Precomputed once at
    /// plan-build time so `get_next` routes ids by direct lookup instead of
    /// re-deriving the name-keyed remap per batch.
    chunk_remaps: Vec<Option<Vec<u32>>>,
}

enum DynamicPlanChild {
    Typed {
        ch_type: ChType,
        plan: Box<FieldExportPlan>,
    },
    Shared,
}

/// Largest Dynamic child set the Arrow export can represent. A Dynamic exports
/// as a two-level dense union whose type codes are signed `i8`. The outer node
/// spends its final code on the intrinsic NULL child, leaving at most
/// `ARROW_UNION_MAX_CHILDREN - 1` non-null groups of `ARROW_UNION_MAX_CHILDREN`
/// typed children. A larger set cannot be routed within the code space, so the
/// stream planner rejects it (127 * 128 = 16,256 children).
const DYNAMIC_MAX_EXPORT_CHILDREN: usize =
    (ARROW_UNION_MAX_CHILDREN - 1) * ARROW_UNION_MAX_CHILDREN;

/// An error from a standalone Arrow C Data export entry point
/// ([`export_batch`], [`export_batch_schema`], [`export_batch_array`]). The
/// Arrow C Stream export ([`export_chunks_to_stream`]) reports the same
/// condition through the stream's `get_last_error` contract instead.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExportError {
    /// A `Dynamic` column's block-local child set is too wide to route within
    /// Arrow's signed Int8 dense-union type-code space. Carries the offending
    /// child count and the export limit ([`DYNAMIC_MAX_EXPORT_CHILDREN`]). This
    /// is reachable from a legitimate server: a FLATTENED Dynamic block
    /// (structure word 3) bounds its runtime type count only by the row count,
    /// not by `max_types`, so a single block can carry more than the limit.
    DynamicUnionTooWide { children: usize, limit: usize },
    /// A `Dynamic` column carries two block-local children with the same
    /// canonical type name. Child type names must be unique: they name the
    /// exported union children and key the stream's result-wide child
    /// unification, so a duplicate would make distinct children
    /// indistinguishable. The decoder and [`DynamicColumn::try_new`] both
    /// enforce uniqueness; this is reachable only through the public
    /// `DynamicColumn` fields.
    DynamicDuplicateChild { name: String },
    /// A `JSON` field's chunks disagree on body kind: some are structured
    /// (typed paths, dynamic paths, shared data) and some are `STRING`-mode
    /// text. The Arrow field format is fixed for a whole stream (`+s` struct vs
    /// `u` utf8), so a mix cannot be exported as one field. The body kind is a
    /// server-result-wide setting, so this only arises from chunks assembled
    /// from different results or hand-built columns.
    JsonBodyKindMismatch,
}

impl std::fmt::Display for ExportError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ExportError::DynamicUnionTooWide { children, limit } => write!(
                f,
                "Dynamic column has {children} distinct types, exceeding the \
                 Arrow union export limit of {limit}"
            ),
            ExportError::DynamicDuplicateChild { name } => write!(
                f,
                "Dynamic column has more than one child named {name}; \
                 child type names must be unique"
            ),
            ExportError::JsonBodyKindMismatch => write!(
                f,
                "JSON column mixes structured and text bodies across chunks; a \
                 JSON field must be all structured or all text in one result"
            ),
        }
    }
}

impl std::error::Error for ExportError {}

/// The distinct child count of the first Dynamic node in `plan` that exceeds
/// [`DYNAMIC_MAX_EXPORT_CHILDREN`], searching recursively through containers and
/// nested Dynamic children. `None` when every Dynamic node fits Arrow's signed
/// Int8 dense-union code space.
fn dynamic_plan_over_limit(plan: &FieldExportPlan) -> Option<usize> {
    match plan {
        FieldExportPlan::Plain => None,
        FieldExportPlan::Array(inner) => dynamic_plan_over_limit(inner),
        FieldExportPlan::Tuple(plans) | FieldExportPlan::Variant(plans) => {
            plans.iter().find_map(dynamic_plan_over_limit)
        }
        FieldExportPlan::Map { key, value } => {
            dynamic_plan_over_limit(key).or_else(|| dynamic_plan_over_limit(value))
        }
        FieldExportPlan::Dynamic(dynamic) => dynamic_export_plan_over_limit(dynamic),
        // A JSON field's typed-path plans recurse like any other field, and each
        // dynamic-path plan carries its own Dynamic union whose per-child budget
        // must fit the signed Int8 code space, exactly as a standalone Dynamic
        // does (the limit is per union node, not summed across paths). Text
        // streams have no Dynamic nodes.
        FieldExportPlan::Json(JsonExportPlan::Text) => None,
        FieldExportPlan::Json(JsonExportPlan::Structured { typed, dynamic, .. }) => {
            typed.iter().find_map(dynamic_plan_over_limit).or_else(|| {
                dynamic
                    .iter()
                    .find_map(|(_, plan)| dynamic_export_plan_over_limit(plan))
            })
        }
    }
}

/// The distinct child count of one Dynamic export plan (or a nested Dynamic
/// below its typed children) that exceeds [`DYNAMIC_MAX_EXPORT_CHILDREN`], or
/// `None` when every node fits Arrow's signed Int8 dense-union code space.
fn dynamic_export_plan_over_limit(dynamic: &DynamicExportPlan) -> Option<usize> {
    if dynamic.children.len() > DYNAMIC_MAX_EXPORT_CHILDREN {
        return Some(dynamic.children.len());
    }
    dynamic.children.iter().find_map(|child| match child {
        DynamicPlanChild::Typed { plan, .. } => dynamic_plan_over_limit(plan),
        DynamicPlanChild::Shared => None,
    })
}

/// Canonical exported child name of one block-local Dynamic child, the same
/// name [`DynamicColumn::try_new`] and the stream's child unification key on.
fn dynamic_child_name(child: &DynamicChild) -> String {
    match child {
        DynamicChild::Typed { ch_type, .. } => ch_type.to_string(),
        DynamicChild::Shared(_) => "SharedVariant".to_string(),
    }
}

/// Validate every Dynamic node in `col` for export, searching recursively
/// through every place the standalone array/schema export descends: dictionary
/// values, list/map elements, tuple fields, Variant alternatives, and Dynamic
/// typed children (including Dynamic nested in Dynamic). This is the
/// column-side twin of the stream's plan-tree checks. Two invariants are
/// enforced: the child set must fit Arrow's signed Int8 dense-union code space
/// ([`DYNAMIC_MAX_EXPORT_CHILDREN`]) and child type names must be unique.
fn check_column_dynamic_export(col: &Column) -> Result<(), ExportError> {
    match col {
        Column::Dynamic(c) => check_dynamic_column_export(c),
        Column::Array(c) => check_column_dynamic_export(&c.values),
        Column::Tuple(c) => c.fields.iter().try_for_each(check_column_dynamic_export),
        Column::Map(c) => check_column_dynamic_export(&c.entries),
        Column::Variant(c) => c.variants.iter().try_for_each(check_column_dynamic_export),
        Column::Dictionary(c) => check_column_dynamic_export(&c.values),
        // A structured JSON column descends exactly where its Arrow export does:
        // each typed-path child column (which can itself be JSON or Dynamic) and
        // each block-local dynamic-path Dynamic column. Shared data is opaque
        // binary and carries no Dynamic. Text JSON has neither.
        Column::Json(c) => {
            c.typed_paths()
                .iter()
                .try_for_each(|(_, values)| check_column_dynamic_export(values))?;
            c.dynamic_paths()
                .iter()
                .try_for_each(|(_, dynamic)| check_dynamic_column_export(dynamic))
        }
        _ => Ok(()),
    }
}

/// Validate one `Dynamic` column for export: the child set must fit Arrow's
/// signed Int8 dense-union code space ([`DYNAMIC_MAX_EXPORT_CHILDREN`]), child
/// type names must be unique, and each typed child recurses. Shared by the
/// `Column::Dynamic` and `Column::Json` (dynamic-path) arms above.
fn check_dynamic_column_export(c: &DynamicColumn) -> Result<(), ExportError> {
    if c.children.len() > DYNAMIC_MAX_EXPORT_CHILDREN {
        return Err(ExportError::DynamicUnionTooWide {
            children: c.children.len(),
            limit: DYNAMIC_MAX_EXPORT_CHILDREN,
        });
    }
    let mut names = std::collections::HashSet::with_capacity(c.children.len());
    for child in &c.children {
        let name = dynamic_child_name(child);
        if !names.insert(name.clone()) {
            return Err(ExportError::DynamicDuplicateChild { name });
        }
    }
    c.children.iter().try_for_each(|child| match child {
        DynamicChild::Typed { values, .. } => check_column_dynamic_export(values),
        DynamicChild::Shared(_) => Ok(()),
    })
}

/// Reject a batch whose Arrow export would emit a malformed Dynamic union: a
/// FLATTENED Dynamic block can legitimately carry more distinct runtime types
/// than the two-level union can route within Arrow's signed Int8 type-code
/// space, and hand-built public `DynamicColumn` fields can carry duplicate
/// child type names. The standalone export entry points fail loudly here
/// rather than emit a schema and array that disagree.
fn check_dynamic_export(batch: &ColBatch) -> Result<(), ExportError> {
    batch
        .columns
        .iter()
        .try_for_each(check_column_dynamic_export)
}

/// Build one field's result-wide export plan from its per-chunk columns.
/// `columns` pairs each column with the index of the chunk it came from
/// (chunks whose column shape does not match are simply absent), so Dynamic
/// nodes can precompute their per-chunk routing tables. Returns
/// [`ExportError::DynamicDuplicateChild`] when a Dynamic column carries two
/// block-local children with the same canonical type name, which would make
/// the name-keyed child unification ambiguous.
fn build_export_plan(
    ch_type: &ChType,
    columns: &[(usize, &Column)],
    num_chunks: usize,
) -> Result<FieldExportPlan, ExportError> {
    if let Some(under) = ch_type.physical_delegate_ref() {
        return build_export_plan(under.as_ref(), columns, num_chunks);
    }
    match ch_type {
        ChType::Nullable(inner) => build_export_plan(inner, columns, num_chunks),
        ChType::Array(inner) => {
            let children = columns
                .iter()
                .filter_map(|&(chunk, column)| match column {
                    Column::Array(column) => Some((chunk, column.values.as_ref())),
                    _ => None,
                })
                .collect::<Vec<_>>();
            Ok(FieldExportPlan::Array(Box::new(build_export_plan(
                inner, &children, num_chunks,
            )?)))
        }
        ChType::Tuple(elements) => {
            let plans = elements
                .iter()
                .enumerate()
                .map(|(index, (_, element_type))| {
                    let children = columns
                        .iter()
                        .filter_map(|&(chunk, column)| match column {
                            Column::Tuple(column) => {
                                column.fields.get(index).map(|field| (chunk, field))
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    build_export_plan(element_type, &children, num_chunks)
                })
                .collect::<Result<Vec<_>, ExportError>>()?;
            Ok(FieldExportPlan::Tuple(plans))
        }
        ChType::Map(key, value) => {
            let mut keys = Vec::new();
            let mut values = Vec::new();
            for &(chunk, column) in columns {
                if let Column::Map(column) = column {
                    if let Column::Tuple(entries) = column.entries.as_ref() {
                        if let [key_column, value_column] = entries.fields.as_slice() {
                            keys.push((chunk, key_column));
                            values.push((chunk, value_column));
                        }
                    }
                }
            }
            Ok(FieldExportPlan::Map {
                key: Box::new(build_export_plan(key, &keys, num_chunks)?),
                value: Box::new(build_export_plan(value, &values, num_chunks)?),
            })
        }
        ChType::Variant(alternatives) => {
            let plans = alternatives
                .iter()
                .enumerate()
                .map(|(index, alternative)| {
                    let children = columns
                        .iter()
                        .filter_map(|&(chunk, column)| match column {
                            Column::Variant(column) => {
                                column.variants.get(index).map(|variant| (chunk, variant))
                            }
                            _ => None,
                        })
                        .collect::<Vec<_>>();
                    build_export_plan(alternative, &children, num_chunks)
                })
                .collect::<Result<Vec<_>, ExportError>>()?;
            Ok(FieldExportPlan::Variant(plans))
        }
        ChType::Dynamic { .. } => {
            let dynamics = columns
                .iter()
                .filter_map(|&(chunk, column)| match column {
                    Column::Dynamic(column) => Some((chunk, column)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            Ok(FieldExportPlan::Dynamic(build_dynamic_plan(
                &dynamics, num_chunks,
            )?))
        }
        ChType::Json { typed_paths, .. } => {
            build_json_plan(typed_paths, columns, num_chunks).map(FieldExportPlan::Json)
        }
        _ => Ok(FieldExportPlan::Plain),
    }
}

/// Build one result-wide [`DynamicExportPlan`] from the per-chunk `Dynamic`
/// columns feeding one field position (a top-level Dynamic field or one JSON
/// dynamic path). `columns` already pairs each present column with its chunk
/// index; a chunk absent from the slice contributes an all-`None` remap so its
/// batch routes every row to the NULL child. Shared by the `ChType::Dynamic`
/// arm and JSON dynamic-path planning so the union unification logic lives in
/// one place.
fn build_dynamic_plan(
    columns: &[(usize, &DynamicColumn)],
    num_chunks: usize,
) -> Result<DynamicExportPlan, ExportError> {
    // Discover the result-wide child set in one pass, mapping each canonical
    // child name to its concrete `ChType` (absent for SharedVariant) and the
    // block-local value columns that feed it. Collecting the value columns here
    // rather than re-scanning per discovered name keeps this linear in the
    // number of children; a result-wide Dynamic can reach tens of thousands of
    // distinct types.
    let mut discovered =
        std::collections::BTreeMap::<String, (Option<ChType>, Vec<(usize, &Column)>)>::new();
    for &(chunk, column) in columns {
        let mut names = std::collections::HashSet::with_capacity(column.children.len());
        for child in &column.children {
            let name = dynamic_child_name(child);
            if !names.insert(name.clone()) {
                return Err(ExportError::DynamicDuplicateChild { name });
            }
            match child {
                DynamicChild::Typed { ch_type, values } => {
                    discovered
                        .entry(name)
                        .or_insert_with(|| (Some(ch_type.clone()), Vec::new()))
                        .1
                        .push((chunk, values));
                }
                DynamicChild::Shared(_) => {
                    discovered.entry(name).or_insert((None, Vec::new()));
                }
            }
        }
    }

    // Result-wide ids follow the BTreeMap's name order. Precompute each chunk's
    // block-local -> result-wide table now so `get_next` never recomputes child
    // names or a name-keyed map per batch.
    let global_by_name: std::collections::HashMap<&str, u32> = discovered
        .keys()
        .enumerate()
        .map(|(global, name)| (name.as_str(), global as u32))
        .collect();
    let mut chunk_remaps: Vec<Option<Vec<u32>>> = vec![None; num_chunks];
    for &(chunk, column) in columns {
        let local_to_global = column
            .children
            .iter()
            .map(|child| {
                // Every child was discovered above; `u32::MAX` is unreachable
                // and would only route rows to NULL.
                global_by_name
                    .get(dynamic_child_name(child).as_str())
                    .copied()
                    .unwrap_or(u32::MAX)
            })
            .collect();
        if let Some(slot) = chunk_remaps.get_mut(chunk) {
            *slot = Some(local_to_global);
        }
    }

    let children = discovered
        .into_values()
        .map(|(ch_type, values)| {
            Ok(match ch_type {
                Some(ch_type) => {
                    let plan = build_export_plan(&ch_type, &values, num_chunks)?;
                    DynamicPlanChild::Typed {
                        ch_type,
                        plan: Box::new(plan),
                    }
                }
                None => DynamicPlanChild::Shared,
            })
        })
        .collect::<Result<Vec<_>, ExportError>>()?;
    Ok(DynamicExportPlan {
        children,
        chunk_remaps,
    })
}

/// Build one result-wide [`JsonExportPlan`] from the per-chunk `JSON` columns
/// feeding one field position. The body kind must agree across the nonempty
/// chunks (see [`ExportError::JsonBodyKindMismatch`]); a zero-row chunk carries
/// no JSON state prefix, so its body kind is whatever `empty_column` built and
/// does not vote. A text result needs no further planning. A structured result
/// recurses `build_export_plan` per typed path and unifies the block-local
/// dynamic path sets by name (BTreeMap order), building one Dynamic plan per
/// result-wide path from the chunks that carry it. A result with no matching
/// nonempty columns at all (a zero-chunk stream) plans as an empty structured
/// body: only the declared typed paths and shared data.
fn build_json_plan(
    typed_paths: &[(String, ChType)],
    columns: &[(usize, &Column)],
    num_chunks: usize,
) -> Result<JsonExportPlan, ExportError> {
    let mut saw_structured = false;
    let mut saw_text = false;
    for &(_, column) in columns {
        if let Column::Json(column) = column {
            if column.is_empty() {
                continue;
            }
            match column.body() {
                JsonBody::Structured(_) => saw_structured = true,
                JsonBody::Text(_) => saw_text = true,
            }
        }
    }
    if saw_structured && saw_text {
        return Err(ExportError::JsonBodyKindMismatch);
    }
    if saw_text {
        return Ok(JsonExportPlan::Text);
    }

    // Structured (also the zero-chunk / all-absent default). Typed-path children
    // stay in the declared ChType order; each recurses through build_export_plan
    // so a typed path that is itself JSON or Dynamic is planned result-wide too.
    let typed = typed_paths
        .iter()
        .enumerate()
        .map(|(index, (_, path_type))| {
            let children = columns
                .iter()
                .filter_map(|&(chunk, column)| match column {
                    Column::Json(column) => column
                        .typed_paths()
                        .get(index)
                        .map(|(_, values)| (chunk, values)),
                    _ => None,
                })
                .collect::<Vec<_>>();
            build_export_plan(path_type, &children, num_chunks)
        })
        .collect::<Result<Vec<_>, ExportError>>()?;

    // Union the block-local dynamic path names across chunks; the BTreeMap key
    // order fixes the exported child order. Each path's Dynamic plan is built
    // from only the chunks that carry it, so a chunk missing the path leaves an
    // all-`None` remap and exports an all-NULL union for it.
    let mut discovered = std::collections::BTreeMap::<String, Vec<(usize, &DynamicColumn)>>::new();
    for &(chunk, column) in columns {
        if let Column::Json(column) = column {
            for (path, dynamic) in column.dynamic_paths() {
                discovered
                    .entry(path.clone())
                    .or_default()
                    .push((chunk, dynamic));
            }
        }
    }
    let dynamic = discovered
        .into_iter()
        .map(|(name, dynamics)| Ok((name, build_dynamic_plan(&dynamics, num_chunks)?)))
        .collect::<Result<Vec<_>, ExportError>>()?;

    Ok(JsonExportPlan::Structured {
        typed_paths: typed_paths.to_vec(),
        typed,
        dynamic,
    })
}

// ---------------------------------------------------------------------------
// Release callbacks
// ---------------------------------------------------------------------------

unsafe extern "C" fn release_schema(schema: *mut ArrowSchema) {
    if schema.is_null() {
        return;
    }
    let s = &mut *schema;
    if s.private_data.is_null() {
        return;
    }
    let pd = Box::from_raw(s.private_data as *mut SchemaPrivateData);
    for child_ptr in &pd.children {
        let child = &mut **child_ptr;
        if let Some(release_fn) = child.release {
            release_fn(*child_ptr);
        }
        let _ = Box::from_raw(*child_ptr);
    }
    // Release and free the dictionary value-type child, if this is a dictionary
    // field. Heap-allocated and owned the same way as the children above.
    if !pd.dictionary.is_null() {
        let dict = &mut *pd.dictionary;
        if let Some(release_fn) = dict.release {
            release_fn(pd.dictionary);
        }
        let _ = Box::from_raw(pd.dictionary);
    }
    drop(pd);
    s.release = None;
    s.private_data = ptr::null_mut();
}

unsafe extern "C" fn release_array(array: *mut ArrowArray) {
    if array.is_null() {
        return;
    }
    let a = &mut *array;
    if a.private_data.is_null() {
        return;
    }
    let pd = Box::from_raw(a.private_data as *mut ArrayPrivateData);
    for child_ptr in &pd.children {
        let child = &mut **child_ptr;
        if let Some(release_fn) = child.release {
            release_fn(*child_ptr);
        }
        let _ = Box::from_raw(*child_ptr);
    }
    // Release and free the dictionary values child, if this is a dictionary
    // column. Heap-allocated and owned the same way as the children above.
    if !pd.dictionary.is_null() {
        let dict = &mut *pd.dictionary;
        if let Some(release_fn) = dict.release {
            release_fn(pd.dictionary);
        }
        let _ = Box::from_raw(pd.dictionary);
    }
    drop(pd);
    a.release = None;
    a.private_data = ptr::null_mut();
}

unsafe extern "C" fn release_stream(stream: *mut ArrowArrayStream) {
    if stream.is_null() {
        return;
    }
    let s = &mut *stream;
    if s.private_data.is_null() {
        return;
    }
    let _ = Box::from_raw(s.private_data as *mut StreamPrivateData);
    s.release = None;
    s.private_data = ptr::null_mut();
}

// ---------------------------------------------------------------------------
// Arrow format strings
// ---------------------------------------------------------------------------

fn arrow_format(ch_type: &ChType) -> String {
    match ch_type {
        // Arrow Null has no data buffers; every row is intrinsically null.
        ChType::Nothing => "n".into(),
        ChType::Bool => "b".into(),
        ChType::Int8 => "c".into(),
        ChType::Int16 => "s".into(),
        ChType::Int32 => "i".into(),
        ChType::Int64 => "l".into(),
        ChType::UInt8 => "C".into(),
        ChType::UInt16 => "S".into(),
        ChType::UInt32 => "I".into(),
        ChType::UInt64 => "L".into(),
        ChType::Float32 => "f".into(),
        ChType::Float64 => "g".into(),
        // Arrow has no native BFloat16 type: `e` is IEEE binary16, whose bit
        // layout differs. Export the raw little-endian word honestly as
        // FixedSizeBinary(2), preserving every bit pattern without a copy.
        ChType::BFloat16 => "w:2".into(),
        // QBit is exposed logically as one fixed-size vector per row. The
        // element type is described by the single child schema; no offsets
        // buffer exists for Arrow FixedSizeList.
        ChType::QBit { dimension, .. } => format!("+w:{dimension}"),
        // Temporal export is zero-copy: never widen or rescale a buffer. Map to
        // a real Arrow temporal type only on an exact same-width match, else
        // expose the raw integer primitive.
        //
        // Date is UInt16 days; Arrow has no 16-bit date, so expose raw days as
        // uint16. Date32 is i32 days, an exact match for Arrow date32. DateTime
        // is UInt32 seconds; Arrow has no u32-seconds timestamp, so expose raw
        // seconds as uint32. DateTime64(P) ticks are i64; an Arrow timestamp is
        // also i64, an exact match, but only for the units Arrow can express
        // (P in {0,3,6,9} -> s/m/u/n). Other precisions fall back to raw i64.
        ChType::Date => "S".into(),
        ChType::Date32 => "tdD".into(),
        ChType::DateTime { .. } => "I".into(),
        ChType::DateTime64 {
            precision,
            timezone,
        } => {
            let unit = match precision {
                0 => "s",
                3 => "m",
                6 => "u",
                9 => "n",
                _ => return "l".into(),
            };
            let tz = timezone.as_deref().unwrap_or("");
            format!("ts{unit}:{tz}")
        }
        // ClickHouse Time permits signed values outside Arrow Time's [0, 24h)
        // domain, so exporting as an Arrow time type would misstate the logical
        // range. Preserve the raw signed seconds/ticks at their native widths,
        // with no validation, rescaling, or copy.
        ChType::Time => "i".into(),
        ChType::Time64 { .. } => "l".into(),
        // Arrow Duration is physically i64 and exactly matches the four units
        // it supports: seconds, milliseconds, microseconds, and nanoseconds.
        // Minute/Hour/Day/Week are fixed counts too, but Arrow has no Duration
        // units for them. Month/Quarter/Year are calendar-variable, and Arrow's
        // calendar interval layouts are physically different. Preserve both
        // groups as raw i64 rather than rescaling or copying.
        ChType::Interval(kind) => match kind {
            IntervalKind::Second => "tDs".into(),
            IntervalKind::Millisecond => "tDm".into(),
            IntervalKind::Microsecond => "tDu".into(),
            IntervalKind::Nanosecond => "tDn".into(),
            IntervalKind::Year
            | IntervalKind::Quarter
            | IntervalKind::Month
            | IntervalKind::Week
            | IntervalKind::Day
            | IntervalKind::Hour
            | IntervalKind::Minute => "l".into(),
        },
        ChType::String => "u".into(),
        // Serialized aggregate states are opaque binary values whose row
        // boundaries were recovered by a function-specific Native codec.
        // LargeBinary is the honest zero-copy Arrow storage: the state can be
        // variable width and has no UTF-8 semantics or 2 GiB aggregate-data cap.
        ChType::AggregateFunction { .. } => "Z".into(),
        ChType::FixedString(n) => format!("w:{n}"),
        // IPv4 is the standard UInt32 numeric value, exported as Arrow uint32
        // (`I`), zero-copy like DateTime. IPv6 and UUID are raw 16-byte blobs,
        // exported as Arrow fixed-size binary of width 16 (`w:16`). The bytes are
        // verbatim wire bytes; this crate does not claim the `arrow.uuid`
        // extension type. Any host value mapping is a binding concern.
        ChType::Ipv4 => "I".into(),
        ChType::Ipv6 => "w:16".into(),
        ChType::Uuid => "w:16".into(),
        // Enum8/Enum16 export as their underlying signed int (Arrow int8 `c` /
        // int16 `s`), zero-copy. Arrow has no native enum and ClickHouse enum
        // values are arbitrary signed ints (not 0..N-1 dictionary indices), so a
        // dictionary export would require forbidden per-cell remapping. The
        // name->value map is carried in the ChType for bindings.
        ChType::Enum8 { .. } => "c".into(),
        ChType::Enum16 { .. } => "s".into(),
        // Decimal(P, S) exports with the Arrow C Data decimal format string
        // `d:precision,scale,bitwidth` over the contiguous little-endian
        // two's-complement buffer, zero-copy. The Arrow C Data spec defines
        // `d:P,S` as 128-bit and `d:P,S,bits` for an explicit bit width, so the
        // 128-bit case is emitted as bare `d:P,S` and the others carry the width.
        // The buffer is exported verbatim at its native width (4/8/16/32 bytes);
        // we deliberately do NOT widen narrow decimals to 128 bits, which would
        // cost a forbidden per-value copy in the hot path.
        //
        // NOTE: `decimal32`/`decimal64`/`decimal256` are newer in the Arrow C
        // Data Interface than `decimal128`. The `d:P,S,bits` spelling is the
        // documented form, but consumer support for the non-128 widths varies by
        // Arrow implementation/version. Confirm consumer compatibility before
        // relying on the 32/64/256-bit exports downstream.
        ChType::Decimal {
            precision,
            scale,
            bits,
        } => {
            if *bits == 128 {
                format!("d:{precision},{scale}")
            } else {
                format!("d:{precision},{scale},{bits}")
            }
        }
        // Wide integers export as Arrow FixedSizeBinary: `w:16` for the 128-bit
        // pair, `w:32` for the 256-bit pair, zero-copy over the verbatim
        // little-endian bytes. NOT the decimal format `d:P,0,bits`: Arrow caps
        // decimal128 at precision 38 and decimal256 at 76, which cannot represent
        // the full 128/256-bit range (2^127-1 is 39 digits), and decimal is
        // signed so an unsigned high-bit value would read as negative.
        // FixedSizeBinary is opaque bytes and universally supported; the binding
        // recovers the integer (and its signedness) from the ChType/type name,
        // exactly as it separates UUID from IPv6, both `w:16`.
        ChType::Int128 | ChType::UInt128 => "w:16".into(),
        ChType::Int256 | ChType::UInt256 => "w:32".into(),
        ChType::Nullable(inner) => arrow_format(inner),
        // A dictionary array's top-level format is the INDEX type. The value
        // type lives in the schema's `dictionary` child. Index width is
        // normalized to i32 by the decoder, so the index format is always `i`.
        ChType::LowCardinality(_) => "i".into(),
        // Array(T) exports as an Arrow LargeList (`+L`, 64-bit offsets), NOT a
        // List (`+l`, 32-bit). The element type is NOT in this format string; it
        // is fully described by a single `item` child schema (see
        // `write_field_schema`). ClickHouse array offsets are `UInt64` element
        // counts and the decoder stores them as `i64` with no artificial i32 cap,
        // so the `offsets: Vec<i64>` buffer is the exact LargeList offsets buffer
        // and is handed over verbatim, zero-copy.
        //
        // NOTE: LargeList consumer support is slightly less universal than
        // List, but is supported by pyarrow, arrow-rs, polars, and duckdb. A
        // `+l` (32-bit) variant would require copying every offset into a fresh
        // i32 buffer (and capping/erroring on element counts above i32::MAX), a
        // per-value copy in the hot path we deliberately avoid. LargeList is
        // the zero-copy match for the i64 offsets the decoder already produces.
        ChType::Array(_) => "+L".into(),
        // Tuple(T1, ...) exports as an Arrow struct (`+s`). The element types
        // are NOT in this format string; each element is a child schema
        // recursively described by `write_field_schema`, named by the
        // ClickHouse element name (verbatim) for a named tuple and by the
        // 1-based decimal position ("1", "2", ...) for an unnamed one. The
        // zero-element `Tuple()` is `+s` with no children. A
        // `Nullable(Tuple(...))` reaches this arm through the `Nullable`
        // recursion above; struct validity is independent of the children per
        // the C Data spec (consumers AND them), so the nullable flag plus the
        // buffers[0] validity bitmap compose like any other nullable column.
        ChType::Tuple(_) => "+s".into(),
        // Map(K, V) exports as LargeList-of-struct (`+L` over an `entries`
        // struct child with `key`/`value` grandchildren), NOT as the Arrow map
        // type `+m`: the C Data spec mandates i32 offsets for `+m` and defines
        // no large-map format, while ClickHouse map offsets are `UInt64` stored
        // as i64, so `+m` would force a per-offset copy and an i32 cap. The
        // chosen naming makes the export shape-isomorphic to Arrow Map minus
        // the offset width, so bindings can cast cheaply.
        // `ARROW_FLAG_MAP_KEYS_SORTED` is never set (it is `+m`-only).
        ChType::Map(..) => "+L".into(),
        // Variant exports as Arrow Dense Union. Up to 127 alternatives plus the
        // implicit NULL child fit in one node. ClickHouse permits 255
        // alternatives, so 128+ use an outer union over groups of at most 128
        // alternatives plus NULL, the Arrow-prescribed union-of-unions shape.
        ChType::Variant(alternatives) => {
            let children = if alternatives.len() < ARROW_UNION_MAX_CHILDREN {
                alternatives.len() + 1
            } else {
                alternatives.len().div_ceil(ARROW_UNION_MAX_CHILDREN) + 1
            };
            dense_union_format(children)
        }
        // Dynamic's child set is block-local and therefore is supplied by the
        // column-aware schema path. A schema with no batch data has only its
        // intrinsic Null child; `export_batch_schema` and Arrow C Stream export
        // replace this with the discovered result-wide union shape.
        ChType::Dynamic { .. } => dense_union_format(1),
        // Name-decoration aliases export with the Arrow shape of the physical
        // type they delegate to (`SimpleAggregateFunction` -> its inner, a geo
        // alias -> its Tuple/Array nesting, `Nested` -> the LargeList over an
        // `Array(Tuple(...))`). The child schemas are emitted by
        // `write_field_schema`, which expands the same delegate.
        ChType::SimpleAggregateFunction { inner, .. } => arrow_format(inner),
        ChType::Geo(kind) => arrow_format(kind.underlying_type_ref()),
        ChType::Geometry => arrow_format(geometry_underlying_type()),
        // `Nested` is always an `Array(Tuple(...))`, so its top format is the
        // LargeList `+L` regardless of the field types (which appear in the
        // `item` struct child), matching the `Array`/`Map` arms above.
        ChType::Nested(_) => "+L".into(),
        // JSON never reaches this generic arm: the schema paths intercept it (as
        // they do Variant and Dynamic) because its shape is column-driven, a
        // structured `+s` struct or a `STRING`-mode `u` utf8 chosen per body.
        // The structured struct is the honest logical default, so return it for
        // any hypothetical direct caller rather than a misleading `n`.
        ChType::Json { .. } => "+s".into(),
    }
}

/// Whether a type exports as an Arrow dictionary array (a `LowCardinality(T)`).
/// Such a type needs the schema/array `dictionary` child populated, unlike the
/// flat types whose `dictionary` pointer stays null.
fn is_dictionary(ch_type: &ChType) -> bool {
    matches!(ch_type, ChType::LowCardinality(_))
}

/// The Arrow value type of a `LowCardinality(T)` dictionary, i.e. the type of
/// the entries in the dictionary `values` column. The inner `Nullable` is
/// transparent here: nulls live in the index validity, so the dictionary value
/// type is always the non-nullable inner type. The shared
/// `low_cardinality_dict_value_type` helper sees through any
/// `SimpleAggregateFunction` chain around the removeNullable `Nullable`, so
/// `LowCardinality(SAF(anyLast, Nullable(String)))` exports the `String` value
/// type rather than a stray alias/`Nullable`.
fn dictionary_value_type(ch_type: &ChType) -> &ChType {
    match ch_type {
        ChType::LowCardinality(inner) => low_cardinality_dict_value_type(inner).1,
        other => other,
    }
}

/// Whether a column exports with the Arrow nullable flag set. A bare
/// `Nullable(T)` is nullable, and a `LowCardinality(Nullable(T))` is nullable
/// at the index level (nulls live in the index validity bitmap). A plain
/// `LowCardinality(T)` is not nullable. The `LowCardinality` case resolves the
/// null flag through the shared `low_cardinality_dict_value_type` helper, so a
/// `LowCardinality(SAF(anyLast, Nullable(String)))` is correctly nullable.
fn field_is_nullable(ch_type: &ChType) -> bool {
    // Most schema writers expand aliases before calling this helper, but keep
    // it correct in isolation too. Geometry delegates to Variant and is
    // intrinsically nullable; the six ordinary geo aliases remain governed by
    // their Tuple/Array shapes.
    if let Some(under) = ch_type.resolved_physical_delegate_ref() {
        return field_is_nullable(under.as_ref());
    }
    match ch_type {
        ChType::Nullable(_) => true,
        // Arrow requires Null-type fields to be nullable: every row is null.
        ChType::Nothing => true,
        ChType::LowCardinality(inner) => low_cardinality_dict_value_type(inner).0,
        // Variant has intrinsic NULL through a Null union child. Arrow unions
        // have no top-level validity bitmap, but the field can still be nullable.
        ChType::Variant(_) => true,
        ChType::Dynamic { .. } => true,
        // JSON is nullable-flagged like Variant/Dynamic. A structured body's
        // struct validity carries a real `Nullable(JSON)` null map; a bare JSON
        // leaves a null validity buffer with null_count 0, which the C Data spec
        // permits for a nullable field.
        ChType::Json { .. } => true,
        _ => false,
    }
}

/// Arrow C Data format string for one dense union node with sequential type
/// codes `0..num_children`.
fn dense_union_format(num_children: usize) -> String {
    let mut format = String::from("+ud:");
    for type_id in 0..num_children {
        if type_id > 0 {
            format.push(',');
        }
        format.push_str(&type_id.to_string());
    }
    format
}

/// Build a C string from a Rust string, dropping any interior NUL bytes.
///
/// Arrow C Data names and format strings are NUL-terminated C strings, so an
/// interior NUL cannot be represented. A column name and a `DateTime`/
/// `DateTime64` timezone both originate from the untrusted wire stream and are
/// only validated as UTF-8, so either can carry a NUL byte. `CString::new`
/// would return `Err` on such input and a `.unwrap()` would panic. That panic
/// can unwind out of the `extern "C"` stream callbacks (`stream_get_schema`),
/// which is undefined behavior across the FFI boundary. Strip the NUL bytes so
/// export stays panic-free; the resulting name/format is best effort for input
/// that is already malformed.
fn cstring_lossy(s: &str) -> CString {
    if s.as_bytes().contains(&0) {
        let cleaned: Vec<u8> = s.bytes().filter(|&b| b != 0).collect();
        // `cleaned` has no interior NUL, so this cannot fail.
        CString::new(cleaned).unwrap_or_default()
    } else {
        CString::new(s).unwrap_or_default()
    }
}

// ---------------------------------------------------------------------------
// Schema export
// ---------------------------------------------------------------------------

unsafe fn write_field_schema(out: *mut ArrowSchema, name: &str, ch_type: &ChType) {
    write_field_schema_for_column(out, name, ch_type, None);
}

/// Column-aware schema export. Dynamic has a block-local child set that is not
/// present in its logical `ChType`, so callers that also have a concrete batch
/// pass the matching column here. All other types retain the ordinary schema
/// path, with the column used only to find a nested Dynamic below a container.
unsafe fn write_field_schema_for_column(
    out: *mut ArrowSchema,
    name: &str,
    ch_type: &ChType,
    column: Option<&Column>,
) {
    // A top-level name-decoration alias (SimpleAggregateFunction, geo, Nested)
    // exports exactly as the physical type it delegates to, so expand and recurse
    // once here. A `Nullable(Point)` is NOT caught here (Nullable has no
    // delegate); its inner geo alias is expanded in the children match below,
    // while `arrow_format` and `field_is_nullable` handle the Nullable wrapper.
    if let Some(under) = ch_type.physical_delegate_ref() {
        write_field_schema_for_column(out, name, under.as_ref(), column);
        return;
    }
    // Resolve aliases below an outer wrapper too. ClickHouse rejects wrappers
    // such as `Nullable(Geometry)`, but `ChType` is public and a hand-built
    // schema must still export a structurally valid Arrow union rather than a
    // `+ud` format with no children.
    let inner_delegate = ch_type.inner().resolved_physical_delegate_ref();
    let inner_type = inner_delegate.as_deref().unwrap_or_else(|| ch_type.inner());
    // Variant carries its own intrinsic NULL through a dedicated Arrow Null
    // union child, and the union field is already flagged nullable, so a
    // `Nullable(Variant)` wrapper adds nothing physical: treat it exactly as a
    // bare Variant. ClickHouse forbids `Nullable(Variant)`
    // (`DataTypeVariant::canBeInsideNullable()` is false, confirmed
    // v26.6.1.1193-stable) and this crate's type parser rejects it, so this only
    // arises from a hand-built `ChType`. Unwrapping the `Nullable` here (exactly
    // as the container children match below does) keeps the schema path from
    // falling through to the generic branch, where `arrow_format` would emit a
    // `+ud` union format string while the children match emitted zero children,
    // a malformed union that strict consumers (pyarrow, arrow-rs) reject. The
    // array path dispatches on `Column::Variant` and is identical for both
    // wrappers, so this keeps the schema and array shapes in agreement.
    if let ChType::Variant(alternatives) = inner_type {
        let variant = match column {
            Some(Column::Variant(col)) => Some(col),
            _ => None,
        };
        write_variant_schema(out, name, alternatives, variant);
        return;
    }
    if let ChType::Dynamic { .. } = inner_type {
        let dynamic = match column {
            Some(Column::Dynamic(col)) => Some(col),
            _ => None,
        };
        write_dynamic_schema(out, name, dynamic);
        return;
    }
    // JSON's shape is column-driven like Variant/Dynamic: a structured body is
    // an Arrow struct whose children are the declared typed paths, the
    // block-local dynamic paths, and the shared-data LargeList, while a
    // STRING-mode body is a plain utf8 column. Both wrappers (`JSON`,
    // `Nullable(JSON)`) reach this via `inner()`; the array path dispatches on
    // `Column::Json` and stays in agreement.
    if let ChType::Json { typed_paths, .. } = inner_type {
        let json = match column {
            Some(Column::Json(col)) => Some(col),
            _ => None,
        };
        write_json_schema(out, name, typed_paths, json);
        return;
    }

    let format = cstring_lossy(&arrow_format(ch_type));
    let name_cstr = cstring_lossy(name);

    // For a dictionary field the value type goes in a `dictionary` child schema;
    // the field's own format string is the index type. The child is allocated
    // and owned by this field's private data so it is freed on release.
    let dictionary = if is_dictionary(ch_type) {
        // Safety: an all-zero `ArrowSchema` is a valid initial value (pointers
        // null, integers zero, `Option<extern fn>` the all-zero `None` niche),
        // and `write_field_schema` overwrites every field before any consumer
        // observes it.
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        write_field_schema_for_column(child, "", dictionary_value_type(ch_type), None);
        child
    } else {
        ptr::null_mut()
    };

    // Container fields describe their element type(s) in child schemas, owned
    // by this field's private data (in `children`, exactly like the top-level
    // record-batch schema owns its column children), so `release_schema`
    // releases and frees them when this field is released. Recursing through
    // `write_field_schema` fully describes each child, including its own
    // nullable flag (`Array(Nullable(T))`), its dictionary child
    // (`Array(LowCardinality(T))`), or a further nested container. The match is
    // on the Nullable-unwrapped value type so a `Nullable(Tuple(...))` still
    // emits its element children (an `Array` is never inside a `Nullable`, so
    // its arm is unaffected by the unwrap).
    let mut children: Vec<*mut ArrowSchema> = Vec::new();
    // Expand a name-decoration alias sitting directly inside a `Nullable`
    // (`Nullable(Point)` -> `Tuple`) so its element children are emitted. A
    // top-level alias was already expanded and recursed above, so this only
    // matters for the one alias legal under `Nullable`, `Point`.
    match inner_type {
        // QBit(T, N) is an Arrow FixedSizeList with one non-nullable scalar
        // child. The decoder already materialized the Native bit planes into
        // this row-major child, so schema export adds no conversion.
        ChType::QBit { element_type, .. } => {
            // Safety: an all-zero ArrowSchema is a valid initial value, and the
            // recursive writer initializes every field before it is exposed.
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            let values = match column {
                Some(Column::QBit(col)) => Some(col.values.as_ref()),
                _ => None,
            };
            write_field_schema_for_column(child, "item", &element_type.ch_type(), values);
            children.push(child);
        }
        // An `Array(T)` LargeList field: one conventionally-named `item` child.
        ChType::Array(inner) => {
            // Safety: an all-zero `ArrowSchema` is a valid initial value, the same
            // niche argument as the dictionary child above; `write_field_schema`
            // overwrites every field before any consumer observes it.
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            let values = match column {
                Some(Column::Array(col)) => Some(col.values.as_ref()),
                _ => None,
            };
            write_field_schema_for_column(child, "item", inner, values);
            children.push(child);
        }
        // A `Tuple(T1, ...)` struct field: one child per element, in declaration
        // order, named by the ClickHouse element name (verbatim; it flows through
        // `cstring_lossy` inside the recursion, like every wire-origin name) or by
        // the 1-based decimal position for an unnamed element. `Tuple()` emits no
        // children.
        ChType::Tuple(elements) => {
            for (i, (element_name, element_type)) in elements.iter().enumerate() {
                // Safety: an all-zero `ArrowSchema` is a valid initial value, the
                // same niche argument as the dictionary child above;
                // `write_field_schema` overwrites every field before any consumer
                // observes it.
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
                let element_column = match column {
                    Some(Column::Tuple(col)) => col.fields.get(i),
                    _ => None,
                };
                match element_name {
                    Some(n) => {
                        write_field_schema_for_column(child, n, element_type, element_column)
                    }
                    None => write_field_schema_for_column(
                        child,
                        &(i + 1).to_string(),
                        element_type,
                        element_column,
                    ),
                }
                children.push(child);
            }
        }
        // A `Map(K, V)` LargeList-of-struct field: one child named `entries`,
        // a non-nullable struct whose `key`/`value` grandchildren describe the
        // key and value types. Synthesizing a transient two-element named
        // `ChType::Tuple` and recursing reuses the Tuple arm above verbatim
        // (the struct format, flags 0, and the recursive key/value description
        // including the value's own nullable flag); the two `ChType` clones are
        // per schema export, never per row.
        ChType::Map(key, value) => {
            let entries_type = ChType::Tuple(vec![
                (Some("key".to_string()), key.as_ref().clone()),
                (Some("value".to_string()), value.as_ref().clone()),
            ]);
            // Safety: an all-zero `ArrowSchema` is a valid initial value, the
            // same niche argument as the dictionary child above;
            // `write_field_schema` overwrites every field before any consumer
            // observes it.
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            let entries = match column {
                Some(Column::Map(col)) => Some(col.entries.as_ref()),
                _ => None,
            };
            write_field_schema_for_column(child, "entries", &entries_type, entries);
            children.push(child);
        }
        _ => {}
    }
    let n_children = children.len() as i64;

    let pd = Box::new(SchemaPrivateData {
        format,
        name: name_cstr,
        children,
        dictionary,
    });

    let schema = &mut *out;
    schema.format = pd.format.as_ptr();
    schema.name = pd.name.as_ptr();
    schema.metadata = ptr::null();
    schema.flags = if field_is_nullable(ch_type) { 2 } else { 0 };
    schema.n_children = n_children;
    // `pd.children` heap buffer is stable across the `Box::into_raw(pd)` move
    // below (moving the Box moves the struct fields, not the Vec's heap buffer),
    // so this pointer stays valid until release. Null when there are no children.
    schema.children = if pd.children.is_empty() {
        ptr::null_mut()
    } else {
        pd.children.as_ptr() as *mut *mut ArrowSchema
    };
    schema.dictionary = pd.dictionary;
    schema.release = Some(release_schema);
    schema.private_data = Box::into_raw(pd) as *mut c_void;
}

/// Write a Variant field as one Arrow Dense Union or, for 128+ alternatives, a
/// two-level union-of-unions. Each alternative child keeps its canonical
/// ClickHouse type name; the final child is Arrow Null and represents Variant's
/// intrinsic discriminator 255.
unsafe fn write_variant_schema(
    out: *mut ArrowSchema,
    name: &str,
    alternatives: &[ChType],
    column: Option<&VariantColumn>,
) {
    let mut children = Vec::new();
    if alternatives.len() < ARROW_UNION_MAX_CHILDREN {
        for (index, alternative) in alternatives.iter().enumerate() {
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            write_field_schema_for_column(
                child,
                &alternative.to_string(),
                alternative,
                column.and_then(|col| col.variants.get(index)),
            );
            children.push(child);
        }
    } else {
        for (group_index, group) in alternatives.chunks(ARROW_UNION_MAX_CHILDREN).enumerate() {
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            let first = group_index * ARROW_UNION_MAX_CHILDREN;
            write_variant_group_schema_with_columns(
                child,
                &format!("variants_{}", group_index + 1),
                group,
                column.map(|col| &col.variants[first..first + group.len()]),
            );
            children.push(child);
        }
    }

    let null_child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
    write_field_schema(null_child, "NULL", &ChType::Nothing);
    children.push(null_child);

    write_union_schema_node(out, name, children, true);
}

/// Write one non-null inner group for a large Variant's Arrow union tree.
unsafe fn write_variant_group_schema_with_columns(
    out: *mut ArrowSchema,
    name: &str,
    alternatives: &[ChType],
    columns: Option<&[Column]>,
) {
    let mut children = Vec::with_capacity(alternatives.len());
    for (index, alternative) in alternatives.iter().enumerate() {
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        write_field_schema_for_column(
            child,
            &alternative.to_string(),
            alternative,
            columns.and_then(|cols| cols.get(index)),
        );
        children.push(child);
    }
    write_union_schema_node(out, name, children, false);
}

/// Write one concrete block-local Dynamic schema. Typed children retain their
/// native Arrow layouts recursively, SharedVariant is honest Arrow Binary (its
/// cells are arbitrary descriptor+payload bytes, not UTF-8), and the final
/// child is Arrow Null. Large child sets use the same union-of-unions shape as
/// Variant so every node stays within Arrow's signed Int8 type-code space.
unsafe fn write_dynamic_schema(out: *mut ArrowSchema, name: &str, column: Option<&DynamicColumn>) {
    let Some(column) = column else {
        let null_child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        write_field_schema(null_child, "NULL", &ChType::Nothing);
        write_union_schema_node(out, name, vec![null_child], true);
        return;
    };
    let mut write_leaf = |index: usize| write_dynamic_child_schema(&column.children[index]);
    write_dynamic_schema_tree(out, name, column.children.len(), &mut write_leaf);
}

/// Write the block-local Dynamic schema as an Arrow Dense Union, in the same
/// two-level shape the array side builds in [`export_dynamic_array`] /
/// [`export_dynamic_array_with_plan`]. A child set that fits one node becomes a
/// flat union of typed children plus the trailing intrinsic-NULL child. A larger
/// set becomes an outer union over non-null groups of at most
/// `ARROW_UNION_MAX_CHILDREN` children (each named `dynamic_i_j` for its 1-based
/// inclusive child range) plus the NULL child.
///
/// The outer union reserves its final type code for NULL, so it holds at most
/// `ARROW_UNION_MAX_CHILDREN - 1` groups and the tree tops out at
/// `DYNAMIC_MAX_EXPORT_CHILDREN` typed children. A block-local Dynamic child set
/// is NOT bounded below that: only the promoted Variant wire form is capped at
/// 254, while a FLATTENED block (structure word 3) bounds its runtime type count
/// only by the row count, so a legitimate server can emit more than the limit in
/// one block. Callers must therefore reject an over-limit child set before
/// reaching this function: the stream path guards its plan in
/// [`export_chunks_to_stream`], and the standalone batch path guards its columns
/// via `check_dynamic_export` in [`export_batch`] / [`export_batch_schema`]
/// / [`export_batch_array`].
unsafe fn write_dynamic_schema_tree<F>(
    out: *mut ArrowSchema,
    name: &str,
    num_children: usize,
    write_leaf: &mut F,
) where
    F: FnMut(usize) -> *mut ArrowSchema,
{
    let mut children = Vec::new();
    if num_children < ARROW_UNION_MAX_CHILDREN {
        children.extend((0..num_children).map(&mut *write_leaf));
    } else {
        let mut start = 0;
        while start < num_children {
            let end = (start + ARROW_UNION_MAX_CHILDREN).min(num_children);
            let group_children = (start..end).map(&mut *write_leaf).collect::<Vec<_>>();
            let group = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            write_union_schema_node(
                group,
                &format!("dynamic_{}_{}", start + 1, end),
                group_children,
                false,
            );
            children.push(group);
            start = end;
        }
    }
    let null_child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
    write_field_schema(null_child, "NULL", &ChType::Nothing);
    children.push(null_child);
    write_union_schema_node(out, name, children, true);
}

unsafe fn write_dynamic_child_schema(child: &DynamicChild) -> *mut ArrowSchema {
    let out = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
    match child {
        DynamicChild::Typed { ch_type, values } => {
            write_field_schema_for_column(out, &ch_type.to_string(), ch_type, Some(values));
        }
        DynamicChild::Shared(_) => write_binary_schema(out, "SharedVariant"),
    }
    out
}

unsafe fn write_binary_schema(out: *mut ArrowSchema, name: &str) {
    let pd = Box::new(SchemaPrivateData {
        format: cstring_lossy("z"),
        name: cstring_lossy(name),
        children: Vec::new(),
        dictionary: ptr::null_mut(),
    });
    let schema = &mut *out;
    schema.format = pd.format.as_ptr();
    schema.name = pd.name.as_ptr();
    schema.metadata = ptr::null();
    schema.flags = 0;
    schema.n_children = 0;
    schema.children = ptr::null_mut();
    schema.dictionary = ptr::null_mut();
    schema.release = Some(release_schema);
    schema.private_data = Box::into_raw(pd) as *mut c_void;
}

unsafe fn write_field_schema_with_plan(
    out: *mut ArrowSchema,
    name: &str,
    ch_type: &ChType,
    plan: &FieldExportPlan,
) {
    if let Some(under) = ch_type.physical_delegate_ref() {
        write_field_schema_with_plan(out, name, under.as_ref(), plan);
        return;
    }
    // Keep the result-wide plan path symmetric with the standalone schema
    // writer for aliases nested below an illegal hand-built wrapper.
    let inner_delegate = ch_type.inner().resolved_physical_delegate_ref();
    let inner_type = inner_delegate.as_deref().unwrap_or_else(|| ch_type.inner());
    if let (ChType::Variant(alternatives), FieldExportPlan::Variant(children)) = (inner_type, plan)
    {
        write_variant_schema_with_plan(out, name, alternatives, children);
        return;
    }
    if let (ChType::Dynamic { .. }, FieldExportPlan::Dynamic(dynamic)) = (inner_type, plan) {
        write_dynamic_schema_with_plan(out, name, dynamic);
        return;
    }
    if let (ChType::Json { typed_paths, .. }, FieldExportPlan::Json(json_plan)) = (inner_type, plan)
    {
        write_json_schema_with_plan(out, name, typed_paths, json_plan);
        return;
    }

    let dictionary = if is_dictionary(ch_type) {
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        write_field_schema(child, "", dictionary_value_type(ch_type));
        child
    } else {
        ptr::null_mut()
    };
    let mut children = Vec::new();
    match (inner_type, plan) {
        (ChType::QBit { element_type, .. }, FieldExportPlan::Plain) => {
            // Safety: an all-zero ArrowSchema is a valid initial value, and the
            // recursive writer initializes every field before it is exposed.
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            write_field_schema(child, "item", &element_type.ch_type());
            children.push(child);
        }
        (ChType::Array(inner), FieldExportPlan::Array(item_plan)) => {
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            write_field_schema_with_plan(child, "item", inner, item_plan);
            children.push(child);
        }
        (ChType::Tuple(elements), FieldExportPlan::Tuple(element_plans)) => {
            for (index, ((element_name, element_type), element_plan)) in
                elements.iter().zip(element_plans).enumerate()
            {
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
                let child_name = element_name
                    .as_deref()
                    .map(str::to_owned)
                    .unwrap_or_else(|| (index + 1).to_string());
                write_field_schema_with_plan(child, &child_name, element_type, element_plan);
                children.push(child);
            }
        }
        (ChType::Map(key, value), FieldExportPlan::Map { key: kp, value: vp }) => {
            let entries = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            let key_schema = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            write_field_schema_with_plan(key_schema, "key", key, kp);
            let value_schema = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            write_field_schema_with_plan(value_schema, "value", value, vp);
            write_struct_schema_node(entries, "entries", vec![key_schema, value_schema], false);
            children.push(entries);
        }
        _ => {}
    }

    write_schema_node(
        out,
        name,
        &arrow_format(ch_type),
        field_is_nullable(ch_type),
        children,
        dictionary,
    );
}

unsafe fn write_variant_schema_with_plan(
    out: *mut ArrowSchema,
    name: &str,
    alternatives: &[ChType],
    plans: &[FieldExportPlan],
) {
    let mut children = Vec::new();
    if alternatives.len() < ARROW_UNION_MAX_CHILDREN {
        for (alternative, plan) in alternatives.iter().zip(plans) {
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            write_field_schema_with_plan(child, &alternative.to_string(), alternative, plan);
            children.push(child);
        }
    } else {
        for (group_index, (types, child_plans)) in alternatives
            .chunks(ARROW_UNION_MAX_CHILDREN)
            .zip(plans.chunks(ARROW_UNION_MAX_CHILDREN))
            .enumerate()
        {
            let group = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            let mut group_children = Vec::with_capacity(types.len());
            for (alternative, plan) in types.iter().zip(child_plans) {
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
                write_field_schema_with_plan(child, &alternative.to_string(), alternative, plan);
                group_children.push(child);
            }
            write_union_schema_node(
                group,
                &format!("variants_{}", group_index + 1),
                group_children,
                false,
            );
            children.push(group);
        }
    }
    let null_child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
    write_field_schema(null_child, "NULL", &ChType::Nothing);
    children.push(null_child);
    write_union_schema_node(out, name, children, true);
}

unsafe fn write_dynamic_schema_with_plan(
    out: *mut ArrowSchema,
    name: &str,
    dynamic: &DynamicExportPlan,
) {
    let mut write_leaf = |index: usize| {
        let child = &dynamic.children[index];
        let out = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        match child {
            DynamicPlanChild::Typed { ch_type, plan } => {
                write_field_schema_with_plan(out, &ch_type.to_string(), ch_type, plan)
            }
            DynamicPlanChild::Shared => write_binary_schema(out, "SharedVariant"),
        }
        out
    };
    write_dynamic_schema_tree(out, name, dynamic.children.len(), &mut write_leaf);
}

/// Write one block-local / standalone `JSON` field schema. A structured body is
/// an Arrow struct (`+s`) with children in the fixed order: one per declared
/// typed path, then one per block-local dynamic path (each a Dynamic union),
/// then the `_shared_data` LargeList. A text body is a plain utf8 (`u`) column.
/// The field is nullable-flagged either way (see [`field_is_nullable`]). With no
/// column (the logical [`export_schema`] path) only the declared typed paths and
/// shared data are known, exactly as Dynamic exposes only its NULL child.
unsafe fn write_json_schema(
    out: *mut ArrowSchema,
    name: &str,
    typed_paths: &[(String, ChType)],
    column: Option<&JsonColumn>,
) {
    let structured = match column {
        // Text body: same Arrow shape as a plain String column.
        Some(column) => match column.body() {
            JsonBody::Text(_) => {
                write_schema_node(out, name, "u", true, Vec::new(), ptr::null_mut());
                return;
            }
            JsonBody::Structured(structured) => Some(structured.as_ref()),
        },
        None => None,
    };

    let mut children = Vec::new();
    for (index, (path, path_type)) in typed_paths.iter().enumerate() {
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        let values =
            structured.and_then(|structured| structured.typed.get(index).map(|(_, values)| values));
        write_field_schema_for_column(child, path, path_type, values);
        children.push(child);
    }
    if let Some(structured) = structured {
        for (path, dynamic) in &structured.dynamic {
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
            write_dynamic_schema(child, path, Some(dynamic));
            children.push(child);
        }
    }
    let shared = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
    write_json_shared_schema(shared, "_shared_data");
    children.push(shared);
    write_struct_schema_node(out, name, children, true);
}

/// Write one result-wide `JSON` field schema from its stream plan. Same fixed
/// child order as [`write_json_schema`], but the dynamic-path children are the
/// unified result-wide set (BTreeMap name order) rather than one block's set.
unsafe fn write_json_schema_with_plan(
    out: *mut ArrowSchema,
    name: &str,
    typed_paths: &[(String, ChType)],
    plan: &JsonExportPlan,
) {
    let (typed, dynamic) = match plan {
        JsonExportPlan::Text => {
            write_schema_node(out, name, "u", true, Vec::new(), ptr::null_mut());
            return;
        }
        JsonExportPlan::Structured { typed, dynamic, .. } => (typed, dynamic),
    };
    let mut children = Vec::new();
    for ((path, path_type), path_plan) in typed_paths.iter().zip(typed) {
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        write_field_schema_with_plan(child, path, path_type, path_plan);
        children.push(child);
    }
    for (path, path_plan) in dynamic {
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        write_dynamic_schema_with_plan(child, path, path_plan);
        children.push(child);
    }
    let shared = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
    write_json_shared_schema(shared, "_shared_data");
    children.push(shared);
    write_struct_schema_node(out, name, children, true);
}

/// Write the `_shared_data` child schema: an Arrow LargeList (`+L`) of a
/// non-nullable struct (`+s`) pairing `paths` (utf8 `u`, like a String column)
/// with `values` (binary `z`, like a SharedVariant cell; the values are opaque
/// descriptor+payload bytes, never utf8). The list is non-nullable, matching
/// how Array export never sets list-level validity.
unsafe fn write_json_shared_schema(out: *mut ArrowSchema, name: &str) {
    let paths = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
    write_schema_node(paths, "paths", "u", false, Vec::new(), ptr::null_mut());
    let values = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
    write_binary_schema(values, "values");
    let item = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
    write_struct_schema_node(item, "item", vec![paths, values], false);
    write_schema_node(out, name, "+L", false, vec![item], ptr::null_mut());
}

unsafe fn write_struct_schema_node(
    out: *mut ArrowSchema,
    name: &str,
    children: Vec<*mut ArrowSchema>,
    nullable: bool,
) {
    write_schema_node(out, name, "+s", nullable, children, ptr::null_mut());
}

unsafe fn write_schema_node(
    out: *mut ArrowSchema,
    name: &str,
    format: &str,
    nullable: bool,
    children: Vec<*mut ArrowSchema>,
    dictionary: *mut ArrowSchema,
) {
    let pd = Box::new(SchemaPrivateData {
        format: cstring_lossy(format),
        name: cstring_lossy(name),
        children,
        dictionary,
    });
    let schema = &mut *out;
    schema.format = pd.format.as_ptr();
    schema.name = pd.name.as_ptr();
    schema.metadata = ptr::null();
    schema.flags = if nullable { 2 } else { 0 };
    schema.n_children = pd.children.len() as i64;
    schema.children = if pd.children.is_empty() {
        ptr::null_mut()
    } else {
        pd.children.as_ptr() as *mut *mut ArrowSchema
    };
    schema.dictionary = pd.dictionary;
    schema.release = Some(release_schema);
    schema.private_data = Box::into_raw(pd) as *mut c_void;
}

/// Finish one Arrow Dense Union schema node and transfer child ownership to its
/// release private data.
unsafe fn write_union_schema_node(
    out: *mut ArrowSchema,
    name: &str,
    children: Vec<*mut ArrowSchema>,
    nullable: bool,
) {
    let format = cstring_lossy(&dense_union_format(children.len()));
    let name = cstring_lossy(name);
    let n_children = children.len() as i64;
    let pd = Box::new(SchemaPrivateData {
        format,
        name,
        children,
        dictionary: ptr::null_mut(),
    });

    let schema = &mut *out;
    schema.format = pd.format.as_ptr();
    schema.name = pd.name.as_ptr();
    schema.metadata = ptr::null();
    schema.flags = if nullable { 2 } else { 0 };
    schema.n_children = n_children;
    schema.children = if pd.children.is_empty() {
        ptr::null_mut()
    } else {
        pd.children.as_ptr() as *mut *mut ArrowSchema
    };
    schema.dictionary = ptr::null_mut();
    schema.release = Some(release_schema);
    schema.private_data = Box::into_raw(pd) as *mut c_void;
}

/// # Safety
///
/// `out` must be a valid, writable pointer to an `ArrowSchema`, normally a
/// zeroed struct. On return `out` owns its data and must be freed through its
/// `release` callback per the Arrow C Data Interface.
///
/// This logical-schema-only API cannot discover the block-local, column-driven
/// shapes of `Dynamic` and `JSON`, so a schema containing either is incomplete
/// here and MUST NOT be paired with [`export_batch_array`]: the resulting
/// schema/array pair would disagree. Use [`export_batch`] or
/// [`export_batch_schema`] for a concrete batch, or the Arrow C Stream API for
/// multiple chunks.
///
/// - `Dynamic` exposes only its intrinsic Null child (no discovered typed
///   children), because the concrete child set lives on the column, not the
///   `ChType`.
/// - `JSON` exposes the structured struct with ONLY the declared typed-path
///   children plus the `_shared_data` child. It cannot show a block's
///   block-local dynamic-path children (which come from the column), and it
///   cannot represent a `STRING`-mode text body (which exports as a `u` utf8
///   array, not a struct), because the body kind is a per-column choice. A
///   concrete JSON array from [`export_batch_array`] may therefore carry extra
///   dynamic-path children or be a utf8 array, either of which mismatches this
///   struct schema.
pub unsafe fn export_schema(schema_in: &Schema, out: *mut ArrowSchema) {
    let n_children = schema_in.num_fields() as i64;

    let mut child_schemas: Vec<*mut ArrowSchema> = Vec::with_capacity(n_children as usize);
    for field in &schema_in.fields {
        // Safety: an all-zero `ArrowSchema` is a valid initial value. Every field
        // is a raw pointer (null is valid), an integer, or `Option<extern fn>`,
        // whose `None` niche is the all-zero bit pattern. `write_field_schema`
        // overwrites every field before the struct is observed by a consumer.
        let child_schema = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        write_field_schema(child_schema, &field.name, &field.ch_type);
        child_schemas.push(child_schema);
    }

    let format = CString::new("+s").unwrap();
    let name = CString::new("").unwrap();

    let pd = Box::new(SchemaPrivateData {
        format,
        name,
        children: child_schemas.clone(),
        dictionary: ptr::null_mut(),
    });

    let schema = &mut *out;
    schema.format = pd.format.as_ptr();
    schema.name = pd.name.as_ptr();
    schema.metadata = ptr::null();
    schema.flags = 0;
    schema.n_children = n_children;
    schema.children = pd.children.as_ptr() as *mut *mut ArrowSchema;
    schema.dictionary = ptr::null_mut();
    schema.release = Some(release_schema);
    schema.private_data = Box::into_raw(pd) as *mut c_void;
}

/// Export the Arrow schema for a concrete record batch. Unlike [`export_schema`],
/// this can describe Dynamic's block-local child union, including a Dynamic
/// nested inside a container. Pair this with [`export_batch_array`] when using
/// the standalone C Data Interface. Arrow C Stream export resolves one common
/// schema across all supplied chunks separately.
///
/// Returns an [`ExportError`] without touching `out` when a Dynamic column's
/// block-local child set exceeds [`DYNAMIC_MAX_EXPORT_CHILDREN`] (reachable
/// from a legitimate FLATTENED block) or carries duplicate child type names
/// (constructible only through the public `DynamicColumn` fields); `out` is
/// then left as the caller passed it so its release stays a no-op.
///
/// # Safety
///
/// `out` must be a valid writable `ArrowSchema`, normally zero-initialized, and
/// the caller must invoke its release callback.
pub unsafe fn export_batch_schema(
    batch: &ColBatch,
    out: *mut ArrowSchema,
) -> Result<(), ExportError> {
    check_dynamic_export(batch)?;
    write_batch_schema(batch, out);
    Ok(())
}

/// Infallible body of [`export_batch_schema`]; callers must have already run
/// [`check_dynamic_export`] on `batch`.
unsafe fn write_batch_schema(batch: &ColBatch, out: *mut ArrowSchema) {
    let mut child_schemas = Vec::with_capacity(batch.schema.num_fields());
    for (field, column) in batch.schema.fields.iter().zip(&batch.columns) {
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        write_field_schema_for_column(child, &field.name, &field.ch_type, Some(column));
        child_schemas.push(child);
    }

    let pd = Box::new(SchemaPrivateData {
        format: cstring_lossy("+s"),
        name: cstring_lossy(""),
        children: child_schemas,
        dictionary: ptr::null_mut(),
    });
    let schema = &mut *out;
    schema.format = pd.format.as_ptr();
    schema.name = pd.name.as_ptr();
    schema.metadata = ptr::null();
    schema.flags = 0;
    schema.n_children = pd.children.len() as i64;
    schema.children = if pd.children.is_empty() {
        ptr::null_mut()
    } else {
        pd.children.as_ptr() as *mut *mut ArrowSchema
    };
    schema.dictionary = ptr::null_mut();
    schema.release = Some(release_schema);
    schema.private_data = Box::into_raw(pd) as *mut c_void;
}

/// Export one concrete batch's Arrow schema and array as an inseparable pair.
/// This is the preferred standalone C Data entry point because the schema and
/// array are derived from the same block-local Dynamic child set.
///
/// Returns [`ExportError::DynamicUnionTooWide`] when a Dynamic column's
/// block-local child set exceeds [`DYNAMIC_MAX_EXPORT_CHILDREN`], or
/// [`ExportError::DynamicDuplicateChild`] when its child type names are not
/// unique. The single check runs before anything is written and the writers it
/// gates are infallible, so on error BOTH `schema_out` and `array_out` are
/// left exactly as the caller passed them (release stays None/zeroed, so caller
/// cleanup is a no-op) and no degenerate all-NULL column is emitted. A block-local
/// Dynamic child set is unbounded only for the FLATTENED wire form; the promoted
/// Variant form is capped at 254 by the decoder and never trips this.
///
/// # Safety
///
/// `schema_out` and `array_out` must be distinct valid writable pointers to
/// zero-initialized Arrow C Data structs. The caller must invoke both release
/// callbacks. `batch` must have one physical column matching every schema field,
/// as required by all Arrow export entry points in this module.
///
/// Every Dynamic column's routing buffers (`type_ids` indexing `children`,
/// `u32::MAX` for NULL) must be internally consistent, as the decoder and
/// encode validation produce them. A column whose public buffers were mutated
/// into an inconsistent state exports as memory-safe but semantically invalid
/// Arrow: a row whose id indexes no child is routed to the NULL child while
/// its dense-union offset still names the slot the id originally selected
/// (see the fallback in [`export_dynamic_array`]).
pub unsafe fn export_batch(
    batch: &Arc<ColBatch>,
    schema_out: *mut ArrowSchema,
    array_out: *mut ArrowArray,
) -> Result<(), ExportError> {
    // Check once up front; the writers below are infallible, so a violation
    // leaves both out-structs untouched and nothing half-exported to release.
    check_dynamic_export(batch)?;
    write_batch_schema(batch, schema_out);
    write_batch_array(batch, array_out);
    Ok(())
}

unsafe fn export_schema_with_plans(
    schema_in: &Schema,
    plans: &[FieldExportPlan],
    out: *mut ArrowSchema,
) {
    let mut children = Vec::with_capacity(schema_in.num_fields());
    for (field, plan) in schema_in.fields.iter().zip(plans) {
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowSchema>()));
        write_field_schema_with_plan(child, &field.name, &field.ch_type, plan);
        children.push(child);
    }
    write_struct_schema_node(out, "", children, false);
}

// ---------------------------------------------------------------------------
// Array export
// ---------------------------------------------------------------------------

unsafe fn export_column_array(batch: &Arc<ColBatch>, col_idx: usize, out: *mut ArrowArray) {
    export_one_column(batch, &batch.columns[col_idx], out);
}

/// Export an arbitrary `Column` into `out`. Recursive: the dictionary `values`
/// column of a `LowCardinality(T)` is exported through the same path into the
/// array's `dictionary` child. `batch` is kept alive in private data so all the
/// borrowed buffers stay valid until release.
unsafe fn export_one_column(batch: &Arc<ColBatch>, col: &Column, out: *mut ArrowArray) {
    let length = col.len() as i64;
    // Arrow Null arrays report every row as null intrinsically; every other
    // column reports its structural null count.
    let null_count = match col {
        Column::Nothing(c) => c.len() as i64,
        // Arrow unions have no top-level validity bitmap or null count. Variant
        // NULL rows route to the explicit Null child instead.
        Column::Variant(_) | Column::Dynamic(_) => 0,
        _ => col.null_count() as i64,
    };

    let mut buffers: Vec<*const c_void> = Vec::new();
    let mut children: Vec<*mut ArrowArray> = Vec::new();
    let mut dictionary: *mut ArrowArray = ptr::null_mut();

    match col {
        // Arrow Null has zero buffers. A `Nullable(Nothing)` column retains its
        // ClickHouse null map for Native re-encoding, but Arrow ignores it and
        // treats every row as null by definition.
        Column::Nothing(_) => {}
        Column::Bool(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            buffers.push(c.bitmap.as_ptr() as *const c_void);
        }
        Column::Int8(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Int16(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Int32(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Int64(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::UInt8(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::UInt16(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::UInt32(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::UInt64(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Float32(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Float64(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::BFloat16(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        // Arrow FixedSizeList has one parent buffer (whole-vector validity) and
        // one flattened child of length rows * dimension, with no offsets.
        // The child borrows the row-major buffer produced by QBit decode.
        Column::QBit(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            // Safety: an all-zero ArrowArray is a valid initial value, and
            // export_one_column initializes every field before it is exposed.
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_one_column(batch, &c.values, child);
            children.push(child);
        }
        Column::Date(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Date32(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::DateTime(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::DateTime64(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Time(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Time64(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Interval(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Utf8(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            push_offsets(&mut buffers, &c.offsets, EMPTY_OFFSETS_I32.as_ptr());
            buffers.push(c.data.as_ptr() as *const c_void);
        }
        // Arrow LargeBinary: validity (always null because ClickHouse forbids
        // Nullable(AggregateFunction)), i64 offsets, then serialized state data.
        // A nullable aggregate ARGUMENT such as sum(Nullable(T)) keeps its
        // presence flag inside each opaque state slice, not in Arrow validity.
        // All three buffers borrow the batch-owned AggregateStateColumn and are
        // kept alive by ArrayPrivateData's Arc<ColBatch>.
        Column::AggregateState(c) => {
            buffers.push(ptr::null());
            push_offsets(&mut buffers, &c.offsets, EMPTY_OFFSETS_I64.as_ptr());
            buffers.push(c.data.as_ptr() as *const c_void);
        }
        Column::FixedBinary(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            buffers.push(c.data.as_ptr() as *const c_void);
        }
        // IPv4 is a uint32 primitive (validity, then values). UUID and IPv6 are
        // width-16 fixed binary (validity, then the contiguous 16-byte rows),
        // pushed exactly like the FixedString arm above.
        Column::Ipv4(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        // Enum8/Enum16 export the underlying signed int buffer (validity, then
        // values), exactly like Int8/Int16.
        Column::Enum8(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        Column::Enum16(c) => push_primitive_buffers(&mut buffers, &c.values, &c.validity),
        // UUID, IPv6, and the wide integers are all width-16/32 fixed-binary
        // buffers exported as 2 buffers (validity, then the contiguous rows),
        // zero-copy. The Arrow format string (`w:16`/`w:32`) is chosen by
        // `arrow_format` from the ChType; the buffers are identical here.
        Column::Ipv6(c)
        | Column::Uuid(c)
        | Column::Int128(c)
        | Column::UInt128(c)
        | Column::Int256(c)
        | Column::UInt256(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            buffers.push(c.data.as_ptr() as *const c_void);
        }
        // Decimal exports 2 buffers (validity, then the contiguous fixed-width
        // little-endian data), exactly like a fixed-size binary buffer of width
        // bits/8. Arrow decimal layout is a single data buffer of the native
        // width, so this is a zero-copy handoff.
        Column::Decimal(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            buffers.push(c.data.as_ptr() as *const c_void);
        }
        Column::Dictionary(c) => {
            // A dictionary array carries the INDEX buffers (validity, then the
            // i32 indices). The dictionary VALUES live in the array's
            // `dictionary` child, exported recursively below.
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            buffers.push(c.indices.as_ptr() as *const c_void);

            // Safety: an all-zero `ArrowArray` is a valid initial value, the
            // same niche argument as the children in `export_batch_array`.
            // `export_one_column` overwrites every field before any consumer
            // observes it.
            let dict_child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_one_column(batch, &c.values, dict_child);
            dictionary = dict_child;
        }
        // Arrow LargeList: 2 buffers in Arrow order, buffer[0] = validity and
        // buffer[1] = the i64 offsets. Arrays are never null at the array level
        // (ClickHouse forbids `Nullable(Array(T))`), so validity is always null.
        // The `offsets` Vec has length `num_rows + 1` with a leading 0 (including
        // the zero-row `[0]` case) and is handed over verbatim, zero-copy. The
        // flattened element column is the single Arrow child, exported
        // recursively so any element type (including `Nullable`,
        // `LowCardinality`, or a further nested `Array`) is fully exported. The
        // child is owned by this array's private data (in `children`), so
        // `release_array` releases and frees it when this array is released. The
        // `_batch` clone keeps the borrowed `offsets` and element buffers alive
        // until release.
        Column::Array(c) => {
            buffers.push(ptr::null());
            push_offsets(&mut buffers, &c.offsets, EMPTY_OFFSETS_I64.as_ptr());

            // Safety: an all-zero `ArrowArray` is a valid initial value, the same
            // niche argument as the `Dictionary` arm's `dict_child` above;
            // `export_one_column` overwrites every field before any consumer
            // observes it.
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_one_column(batch, &c.values, child);
            children.push(child);
        }
        // Arrow struct (`+s`): 1 buffer, the validity slot. Populated only for
        // a `Nullable(Tuple(...))` (struct validity is independent of the
        // children per the C Data spec; consumers AND them); a plain Tuple
        // pushes null with null_count 0, which the spec permits. One child per
        // element column, in declaration order, exported recursively so any
        // element type (`Nullable`, `LowCardinality`, `Array`, a nested
        // `Tuple`) composes; a zero-element `Tuple()` has no children and its
        // length comes from the explicit `TupleColumn::len`. The children are
        // owned by this array's private data (in `children`), so
        // `release_array` releases and frees them, and the `_batch` clone keeps
        // every borrowed element buffer alive until release.
        Column::Tuple(c) => {
            match &c.validity {
                Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
                None => buffers.push(ptr::null()),
            }
            for element in &c.fields {
                // Safety: an all-zero `ArrowArray` is a valid initial value, the
                // same niche argument as the `Dictionary` arm's `dict_child`
                // above; `export_one_column` overwrites every field before any
                // consumer observes it.
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                export_one_column(batch, element, child);
                children.push(child);
            }
        }
        // Map(K, V) exports byte-identically to the `Array` arm above: 2
        // buffers (validity, always null here since a map is never nullable at
        // the map level, then the i64 offsets handed over verbatim) and one
        // child, the flattened `entries` tuple column, exported recursively
        // (its own Tuple arm yields the struct node with the key and value
        // children). Ownership follows the same private-data pattern.
        Column::Map(c) => {
            buffers.push(ptr::null());
            push_offsets(&mut buffers, &c.offsets, EMPTY_OFFSETS_I64.as_ptr());

            // Safety: an all-zero `ArrowArray` is a valid initial value, the
            // same niche argument as the `Dictionary` arm's `dict_child` above;
            // `export_one_column` overwrites every field before any consumer
            // observes it.
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_one_column(batch, &c.entries, child);
            children.push(child);
        }
        // Arrow Dense Union: exactly two buffers, signed Int8 type ids then i32
        // child offsets, with no validity buffer. The decoder built these buffers
        // directly, so export is zero-copy. A large Variant's outer children are
        // inner union groups; a small Variant's children are the dense alternative
        // columns directly. The final child is always Arrow Null.
        //
        // Dispatch is on the `Column`, not the schema `ChType`, so a
        // `Nullable(Variant)` wrapper (impossible from parsed input; ClickHouse
        // forbids it via `canBeInsideNullable() == false`, and the type parser
        // rejects it) produces this identical union with no top-level validity.
        // That matches the schema path, which unwraps the same `Nullable` and
        // emits a bare Variant union, so the two shapes can never disagree.
        Column::Variant(c) => {
            match &c.layout {
                VariantLayout::Flat { type_ids, offsets } => {
                    buffers.push(type_ids.as_ptr() as *const c_void);
                    buffers.push(offsets.as_ptr() as *const c_void);
                    for variant in &c.variants {
                        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                        export_one_column(batch, variant, child);
                        children.push(child);
                    }
                }
                VariantLayout::Nested {
                    type_ids,
                    offsets,
                    groups,
                } => {
                    buffers.push(type_ids.as_ptr() as *const c_void);
                    buffers.push(offsets.as_ptr() as *const c_void);
                    for group in groups {
                        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                        export_variant_group_array(batch, c, group, child);
                        children.push(child);
                    }
                }
            }

            let null_child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_variant_null_array(batch, c, null_child);
            children.push(null_child);
        }
        Column::Dynamic(c) => {
            export_dynamic_array(batch, c, out);
            return;
        }
        // JSON is column-driven and writes its whole node here, exactly like
        // Dynamic above: a structured body becomes an Arrow struct, a text body
        // a plain utf8 column.
        Column::Json(c) => {
            export_json_array(batch, c, out);
            return;
        }
    }
    write_array_node(
        batch, out, length, null_count, buffers, children, dictionary,
    );
}

/// Export one block-local Dynamic column as an Arrow Dense Union. Its u32 local
/// ids are remapped once into Arrow signed Int8 routing. Typed child buffers and
/// flat-layout offsets remain borrowed zero-copy; 128+ child sets synthesize the
/// small outer/group routing needed by the union-of-unions representation.
unsafe fn export_dynamic_array(
    batch: &Arc<ColBatch>,
    dynamic: &DynamicColumn,
    out: *mut ArrowArray,
) {
    // Route a block-local id at or beyond `children.len()` to the NULL child,
    // the same clamp the plan path applies in `export_dynamic_array_with_plan`.
    // `DynamicColumn`'s fields are public, so safe code can construct ids that
    // index no child; without the clamp the <128 branch would emit a wrapped
    // bogus union code and the grouped branch would index out of bounds inside
    // a pub unsafe fn reachable from `extern "C"`. The result for such a row is
    // memory-safe but semantically invalid Arrow (its offset still names the
    // slot the id originally selected); valid routing is a documented
    // precondition of the standalone entry points, and debug builds trip a
    // deliberate assert exactly like the plan path's fallback.
    let resolve_child = |type_id: u32| -> usize {
        if type_id != u32::MAX && (type_id as usize) < dynamic.children.len() {
            return type_id as usize;
        }
        debug_assert!(
            type_id == u32::MAX,
            "Dynamic local id {type_id} indexes no child; routing buffers are \
             inconsistent with the child set"
        );
        usize::MAX
    };

    if dynamic.children.len() < ARROW_UNION_MAX_CHILDREN {
        let null_id = dynamic.children.len() as i8;
        let type_ids = dynamic
            .type_ids
            .iter()
            .map(|&type_id| match resolve_child(type_id) {
                usize::MAX => null_id,
                child => child as i8,
            })
            .collect();
        let mut children = dynamic
            .children
            .iter()
            .map(|child| export_dynamic_child_array(batch, child))
            .collect::<Vec<_>>();
        children.push(export_dynamic_null_array(batch, dynamic));
        write_owned_union_array_node(
            batch,
            out,
            dynamic.len(),
            type_ids,
            dynamic.offsets.as_ptr(),
            Vec::new(),
            children,
        );
        return;
    }

    let num_groups = dynamic.children.len().div_ceil(ARROW_UNION_MAX_CHILDREN);
    let mut outer_ids = Vec::with_capacity(dynamic.len());
    let mut outer_offsets = Vec::with_capacity(dynamic.len());
    // Size each group's routing buffers exactly with a counts prepass so the
    // per-row loop below never reallocates.
    let mut group_row_counts = vec![0usize; num_groups];
    for &type_id in &dynamic.type_ids {
        if type_id != u32::MAX && (type_id as usize) < dynamic.children.len() {
            group_row_counts[type_id as usize / ARROW_UNION_MAX_CHILDREN] += 1;
        }
    }
    let mut group_ids = group_row_counts
        .iter()
        .map(|&rows| Vec::with_capacity(rows))
        .collect::<Vec<_>>();
    let mut group_offsets = group_row_counts
        .iter()
        .map(|&rows| Vec::with_capacity(rows))
        .collect::<Vec<_>>();
    let mut group_counts = vec![0i32; num_groups];
    let mut null_count = 0i32;

    for (&type_id, &child_offset) in dynamic.type_ids.iter().zip(&dynamic.offsets) {
        let child = resolve_child(type_id);
        if child == usize::MAX {
            outer_ids.push(num_groups as i8);
            outer_offsets.push(null_count);
            null_count += 1;
            continue;
        }
        let group = child / ARROW_UNION_MAX_CHILDREN;
        outer_ids.push(group as i8);
        outer_offsets.push(group_counts[group]);
        group_counts[group] += 1;
        group_ids[group].push((child % ARROW_UNION_MAX_CHILDREN) as i8);
        group_offsets[group].push(child_offset);
    }

    let mut children = Vec::with_capacity(num_groups + 1);
    for group in 0..num_groups {
        let first = group * ARROW_UNION_MAX_CHILDREN;
        let end = (first + ARROW_UNION_MAX_CHILDREN).min(dynamic.children.len());
        let group_children = dynamic.children[first..end]
            .iter()
            .map(|child| export_dynamic_child_array(batch, child))
            .collect();
        let group_array = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
        let ids = std::mem::take(&mut group_ids[group]);
        let offsets = std::mem::take(&mut group_offsets[group]);
        write_owned_union_array_node(
            batch,
            group_array,
            ids.len(),
            ids,
            ptr::null(),
            offsets,
            group_children,
        );
        children.push(group_array);
    }
    children.push(export_dynamic_null_array(batch, dynamic));
    write_owned_union_array_node(
        batch,
        out,
        dynamic.len(),
        outer_ids,
        ptr::null(),
        outer_offsets,
        children,
    );
}

unsafe fn export_dynamic_array_with_plan(
    batch: &Arc<ColBatch>,
    dynamic: &DynamicColumn,
    plan: &DynamicExportPlan,
    chunk: usize,
    out: *mut ArrowArray,
) {
    // The block-local -> result-wide table was precomputed at plan-build time
    // (see `build_export_plan`), so no child names or name-keyed maps are
    // rebuilt per batch. A missing table (the column at this position was not
    // a Dynamic when the plan was built) leaves every id unresolved, routing
    // all rows through the NULL fallback below.
    let local_to_global: &[u32] = plan
        .chunk_remaps
        .get(chunk)
        .and_then(|remap| remap.as_deref())
        .unwrap_or(&[]);
    let mut global_to_local: Vec<Option<usize>> = vec![None; plan.children.len()];
    for (local, &global) in local_to_global.iter().enumerate() {
        if let Some(slot) = global_to_local.get_mut(global as usize) {
            *slot = Some(local);
        }
    }

    // Resolve one block-local id to its result-wide (global) child index, or
    // u32::MAX for NULL and for an unplanned id (see the fallback note below).
    // Kept as a shared closure so each width branch walks `dynamic.type_ids`
    // exactly once, without materializing an intermediate global-id run for the
    // common <128-child branch. This mirrors the sibling `export_dynamic_array`,
    // which likewise builds its Arrow type-id run in a single pass.
    let resolve_global = |local_id: u32| -> u32 {
        if local_id == u32::MAX {
            return u32::MAX;
        }
        match local_to_global.get(local_id as usize) {
            Some(&global) => global,
            None => {
                // Well-formed Arrow export requires every block-local id to
                // resolve to a planned child, and it always does for decoded
                // columns and encode-validated input because the plan is the
                // union of every chunk's child set. A caller that mutates
                // Dynamic's public routing buffers (`type_ids`, `children`) after
                // the plan was fixed can break that. Route the unresolved row to
                // the NULL child so this unsafe FFI path stays panic-free and
                // memory-safe.
                //
                // The dense-union offset for this row still comes from
                // `dynamic.offsets` (borrowed zero-copy in the <128 branch below),
                // so it indexes the NULL child rather than a valid slot: the
                // result is memory-safe but semantically invalid Arrow. Valid
                // routing is an unsafe precondition of Dynamic export (see
                // `export_chunks_to_stream`); we flag the violation in debug builds
                // instead of copying the offsets buffer on the well-formed
                // zero-copy path.
                //
                // This callback is reachable from `extern "C"` stream callbacks.
                // A panic across that boundary is UB, but a `debug_assert!` panic
                // in a `panic = "abort"`-agnostic sense is fine here: the crate is
                // compiled with the default unwind runtime, and a panic that would
                // cross an `extern "C"` frame aborts the process rather than
                // unwinding into the host (Rust makes `extern "C"` frames abort on
                // unwind). So in debug builds this is a deliberate hard tripwire
                // (defined process abort) for an embedding host that mutated the
                // routing buffers; release builds silently take the safe fallback.
                debug_assert!(
                    false,
                    "Dynamic local id {local_id} has no planned export child; \
                     routing buffers were mutated after the export plan was built"
                );
                u32::MAX
            }
        }
    };

    let export_children = |range: std::ops::Range<usize>| {
        range
            .map(|global| {
                // `plan.children` and `global_to_local` are plan-build-time
                // tables sized to the plan, and `range` covers plan child
                // indices, so indexing them is in bounds. Only the block's own
                // child set can shrink afterwards; `.get` degrades a missing
                // child to an empty child instead of indexing out of bounds
                // under `extern "C"`.
                export_dynamic_plan_child_array(
                    batch,
                    &plan.children[global],
                    global_to_local[global].and_then(|local| dynamic.children.get(local)),
                    chunk,
                )
            })
            .collect::<Vec<_>>()
    };

    if plan.children.len() < ARROW_UNION_MAX_CHILDREN {
        // Common case: map each block-local id straight to its signed Arrow type
        // id in one pass. NULL and any unresolved row route to the trailing Null
        // child. No intermediate u32 global-id buffer is built.
        let null_id = plan.children.len() as i8;
        let type_ids = dynamic
            .type_ids
            .iter()
            .map(|&local_id| {
                let global = resolve_global(local_id);
                if global == u32::MAX {
                    null_id
                } else {
                    global as i8
                }
            })
            .collect();
        let mut children = export_children(0..plan.children.len());
        children.push(export_dynamic_null_array(batch, dynamic));
        write_owned_union_array_node(
            batch,
            out,
            dynamic.len(),
            type_ids,
            dynamic.offsets.as_ptr(),
            Vec::new(),
            children,
        );
        return;
    }

    // 128+ planned children need the union-of-unions remap, which reads the full
    // u32 global-id run below; build it once here.
    let global_ids: Vec<u32> = dynamic
        .type_ids
        .iter()
        .map(|&local_id| resolve_global(local_id))
        .collect();

    let num_groups = plan.children.len().div_ceil(ARROW_UNION_MAX_CHILDREN);
    let mut outer_ids = Vec::with_capacity(dynamic.len());
    let mut outer_offsets = Vec::with_capacity(dynamic.len());
    // Size each group's routing buffers exactly with a counts prepass so the
    // per-row loop below never reallocates.
    let mut group_row_counts = vec![0usize; num_groups];
    for &global_id in &global_ids {
        if global_id != u32::MAX {
            group_row_counts[global_id as usize / ARROW_UNION_MAX_CHILDREN] += 1;
        }
    }
    let mut group_ids = group_row_counts
        .iter()
        .map(|&rows| Vec::with_capacity(rows))
        .collect::<Vec<_>>();
    let mut group_offsets = group_row_counts
        .iter()
        .map(|&rows| Vec::with_capacity(rows))
        .collect::<Vec<_>>();
    let mut group_counts = vec![0i32; num_groups];
    let mut null_count = 0i32;
    for (&global_id, &child_offset) in global_ids.iter().zip(&dynamic.offsets) {
        if global_id == u32::MAX {
            outer_ids.push(num_groups as i8);
            outer_offsets.push(null_count);
            null_count += 1;
            continue;
        }
        let child = global_id as usize;
        let group = child / ARROW_UNION_MAX_CHILDREN;
        outer_ids.push(group as i8);
        outer_offsets.push(group_counts[group]);
        group_counts[group] += 1;
        group_ids[group].push((child % ARROW_UNION_MAX_CHILDREN) as i8);
        group_offsets[group].push(child_offset);
    }

    let mut children = Vec::with_capacity(num_groups + 1);
    for group in 0..num_groups {
        let first = group * ARROW_UNION_MAX_CHILDREN;
        let end = (first + ARROW_UNION_MAX_CHILDREN).min(plan.children.len());
        let group_children = export_children(first..end);
        let group_array = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
        let ids = std::mem::take(&mut group_ids[group]);
        let offsets = std::mem::take(&mut group_offsets[group]);
        write_owned_union_array_node(
            batch,
            group_array,
            ids.len(),
            ids,
            ptr::null(),
            offsets,
            group_children,
        );
        children.push(group_array);
    }
    children.push(export_dynamic_null_array(batch, dynamic));
    write_owned_union_array_node(
        batch,
        out,
        dynamic.len(),
        outer_ids,
        ptr::null(),
        outer_offsets,
        children,
    );
}

unsafe fn export_dynamic_plan_child_array(
    batch: &Arc<ColBatch>,
    plan: &DynamicPlanChild,
    local: Option<&DynamicChild>,
    chunk: usize,
) -> *mut ArrowArray {
    let out = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    match (plan, local) {
        (DynamicPlanChild::Typed { plan, .. }, Some(DynamicChild::Typed { values, .. })) => {
            export_one_column_with_plan(batch, values, plan, chunk, out)
        }
        (DynamicPlanChild::Shared, Some(DynamicChild::Shared(values))) => {
            export_binary_array(batch, values, out)
        }
        (DynamicPlanChild::Typed { ch_type, plan }, None) => {
            export_empty_column_with_plan(batch, ch_type, plan, chunk, out)
        }
        (DynamicPlanChild::Shared, None) => export_empty_binary_array(batch, out),
        _ => export_empty_binary_array(batch, out),
    }
    out
}

unsafe fn export_empty_binary_array(batch: &Arc<ColBatch>, out: *mut ArrowArray) {
    write_array_node(
        batch,
        out,
        0,
        0,
        vec![
            ptr::null(),
            EMPTY_OFFSETS_I32.as_ptr() as *const c_void,
            ptr::null(),
        ],
        Vec::new(),
        ptr::null_mut(),
    );
}

unsafe fn export_empty_column_with_plan(
    batch: &Arc<ColBatch>,
    ch_type: &ChType,
    plan: &FieldExportPlan,
    chunk: usize,
    out: *mut ArrowArray,
) {
    if let Some(under) = ch_type.physical_delegate_ref() {
        export_empty_column_with_plan(batch, under.as_ref(), plan, chunk, out);
        return;
    }
    if let ChType::Nullable(inner) = ch_type {
        export_empty_column_with_plan(batch, inner, plan, chunk, out);
        return;
    }
    match (ch_type, plan) {
        (ChType::QBit { element_type, .. }, FieldExportPlan::Plain) => {
            // Safety: an all-zero ArrowArray is a valid initial value, and the
            // recursive exporter initializes every field before it is exposed.
            let item = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_empty_column_with_plan(
                batch,
                &element_type.ch_type(),
                &FieldExportPlan::Plain,
                chunk,
                item,
            );
            write_array_node(
                batch,
                out,
                0,
                0,
                vec![ptr::null()],
                vec![item],
                ptr::null_mut(),
            );
        }
        (ChType::Array(inner), FieldExportPlan::Array(item_plan)) => {
            let item = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_empty_column_with_plan(batch, inner, item_plan, chunk, item);
            write_array_node(
                batch,
                out,
                0,
                0,
                vec![ptr::null(), EMPTY_OFFSETS_I64.as_ptr() as *const c_void],
                vec![item],
                ptr::null_mut(),
            );
        }
        (ChType::Tuple(elements), FieldExportPlan::Tuple(plans)) => {
            let mut children = Vec::with_capacity(elements.len());
            for ((_, element_type), element_plan) in elements.iter().zip(plans) {
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                export_empty_column_with_plan(batch, element_type, element_plan, chunk, child);
                children.push(child);
            }
            write_array_node(
                batch,
                out,
                0,
                0,
                vec![ptr::null()],
                children,
                ptr::null_mut(),
            );
        }
        (ChType::Map(key, value), FieldExportPlan::Map { key: kp, value: vp }) => {
            let key_array = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_empty_column_with_plan(batch, key, kp, chunk, key_array);
            let value_array = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_empty_column_with_plan(batch, value, vp, chunk, value_array);
            let entries = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            write_array_node(
                batch,
                entries,
                0,
                0,
                vec![ptr::null()],
                vec![key_array, value_array],
                ptr::null_mut(),
            );
            write_array_node(
                batch,
                out,
                0,
                0,
                vec![ptr::null(), EMPTY_OFFSETS_I64.as_ptr() as *const c_void],
                vec![entries],
                ptr::null_mut(),
            );
        }
        (ChType::Variant(alternatives), FieldExportPlan::Variant(plans)) => {
            export_empty_variant_with_plan(batch, alternatives, plans, chunk, out);
        }
        (ChType::Dynamic { .. }, FieldExportPlan::Dynamic(dynamic)) => {
            export_empty_dynamic_with_plan(batch, dynamic, chunk, out);
        }
        (ChType::Json { typed_paths, .. }, FieldExportPlan::Json(json_plan)) => {
            export_empty_json_with_plan(batch, typed_paths, json_plan, chunk, out);
        }
        (ChType::LowCardinality(inner), _) => {
            let dictionary = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_empty_column_with_plan(
                batch,
                low_cardinality_dict_value_type(inner).1,
                &FieldExportPlan::Plain,
                chunk,
                dictionary,
            );
            write_array_node(
                batch,
                out,
                0,
                0,
                vec![ptr::null(), ptr::null()],
                Vec::new(),
                dictionary,
            );
        }
        (ChType::Nothing, _) => {
            write_array_node(batch, out, 0, 0, Vec::new(), Vec::new(), ptr::null_mut())
        }
        (ChType::String, _) => export_empty_binary_array(batch, out),
        (ChType::AggregateFunction { .. }, _) => write_array_node(
            batch,
            out,
            0,
            0,
            vec![
                ptr::null(),
                EMPTY_OFFSETS_I64.as_ptr() as *const c_void,
                ptr::null(),
            ],
            Vec::new(),
            ptr::null_mut(),
        ),
        _ => write_array_node(
            batch,
            out,
            0,
            0,
            vec![ptr::null(), ptr::null()],
            Vec::new(),
            ptr::null_mut(),
        ),
    }
}

unsafe fn export_empty_variant_with_plan(
    batch: &Arc<ColBatch>,
    alternatives: &[ChType],
    plans: &[FieldExportPlan],
    chunk: usize,
    out: *mut ArrowArray,
) {
    let child_arrays = || {
        alternatives
            .iter()
            .zip(plans)
            .map(|(ch_type, plan)| {
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                export_empty_column_with_plan(batch, ch_type, plan, chunk, child);
                child
            })
            .collect::<Vec<_>>()
    };
    let mut children = if alternatives.len() < ARROW_UNION_MAX_CHILDREN {
        child_arrays()
    } else {
        let all = child_arrays();
        let mut groups = Vec::new();
        for child_group in all.chunks(ARROW_UNION_MAX_CHILDREN) {
            let group = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            write_array_node(
                batch,
                group,
                0,
                0,
                vec![ptr::null(), ptr::null()],
                child_group.to_vec(),
                ptr::null_mut(),
            );
            groups.push(group);
        }
        groups
    };
    let null = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    write_array_node(batch, null, 0, 0, Vec::new(), Vec::new(), ptr::null_mut());
    children.push(null);
    write_array_node(
        batch,
        out,
        0,
        0,
        vec![ptr::null(), ptr::null()],
        children,
        ptr::null_mut(),
    );
}

unsafe fn export_empty_dynamic_with_plan(
    batch: &Arc<ColBatch>,
    dynamic: &DynamicExportPlan,
    chunk: usize,
    out: *mut ArrowArray,
) {
    let all = dynamic
        .children
        .iter()
        .map(|child| export_dynamic_plan_child_array(batch, child, None, chunk))
        .collect::<Vec<_>>();
    let mut children = if dynamic.children.len() < ARROW_UNION_MAX_CHILDREN {
        all
    } else {
        let mut groups = Vec::new();
        for child_group in all.chunks(ARROW_UNION_MAX_CHILDREN) {
            let group = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            write_array_node(
                batch,
                group,
                0,
                0,
                vec![ptr::null(), ptr::null()],
                child_group.to_vec(),
                ptr::null_mut(),
            );
            groups.push(group);
        }
        groups
    };
    let null = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    write_array_node(batch, null, 0, 0, Vec::new(), Vec::new(), ptr::null_mut());
    children.push(null);
    write_array_node(
        batch,
        out,
        0,
        0,
        vec![ptr::null(), ptr::null()],
        children,
        ptr::null_mut(),
    );
}

/// Export a zero-length `JSON` node matching its result-wide plan, used when a
/// JSON column is a Dynamic typed child absent from one chunk. Structured emits
/// the empty struct (empty typed children, empty dynamic unions, empty
/// `_shared_data`); text emits an empty utf8 column.
unsafe fn export_empty_json_with_plan(
    batch: &Arc<ColBatch>,
    typed_paths: &[(String, ChType)],
    plan: &JsonExportPlan,
    chunk: usize,
    out: *mut ArrowArray,
) {
    let (typed, dynamic) = match plan {
        JsonExportPlan::Text => {
            export_empty_binary_array(batch, out);
            return;
        }
        JsonExportPlan::Structured { typed, dynamic, .. } => (typed, dynamic),
    };
    let mut children = Vec::new();
    for ((_, path_type), path_plan) in typed_paths.iter().zip(typed) {
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
        export_empty_column_with_plan(batch, path_type, path_plan, chunk, child);
        children.push(child);
    }
    for (_, path_plan) in dynamic {
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
        export_empty_dynamic_with_plan(batch, path_plan, chunk, child);
        children.push(child);
    }
    // Empty _shared_data: a zero-row LargeList over an empty (paths, values)
    // struct, matching the shared schema shape with no borrowed data.
    let paths = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    export_empty_binary_array(batch, paths);
    let values = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    export_empty_binary_array(batch, values);
    let item = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    write_array_node(
        batch,
        item,
        0,
        0,
        vec![ptr::null()],
        vec![paths, values],
        ptr::null_mut(),
    );
    let shared = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    write_array_node(
        batch,
        shared,
        0,
        0,
        vec![ptr::null(), EMPTY_OFFSETS_I64.as_ptr() as *const c_void],
        vec![item],
        ptr::null_mut(),
    );
    children.push(shared);
    write_array_node(
        batch,
        out,
        0,
        0,
        vec![ptr::null()],
        children,
        ptr::null_mut(),
    );
}

unsafe fn export_dynamic_child_array(
    batch: &Arc<ColBatch>,
    child: &DynamicChild,
) -> *mut ArrowArray {
    let out = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    match child {
        DynamicChild::Typed { values, .. } => export_one_column(batch, values, out),
        DynamicChild::Shared(values) => export_binary_array(batch, values, out),
    }
    out
}

unsafe fn export_binary_array(batch: &Arc<ColBatch>, values: &Utf8Column, out: *mut ArrowArray) {
    let mut buffers = vec![ptr::null()];
    push_offsets(&mut buffers, &values.offsets, EMPTY_OFFSETS_I32.as_ptr());
    buffers.push(values.data.as_ptr() as *const c_void);
    write_array_node(
        batch,
        out,
        values.len() as i64,
        0,
        buffers,
        Vec::new(),
        ptr::null_mut(),
    );
}

/// The struct/utf8 validity buffer for one `JSON` column: the `Nullable(JSON)`
/// null map when present, else a null pointer (all valid). A bare JSON leaves
/// this null with null_count 0.
fn json_validity_buffer(column: &JsonColumn) -> *const c_void {
    column.validity.as_ref().map_or(ptr::null(), |bitmap| {
        bitmap.as_bytes().as_ptr() as *const c_void
    })
}

/// Export one standalone / block-local `JSON` column. A structured body becomes
/// an Arrow struct whose children are the typed paths, the block-local dynamic
/// paths (each a Dynamic union), then `_shared_data`; a text body becomes a
/// plain utf8 column. Every buffer is borrowed from the batch-owned column and
/// kept alive by the `Arc<ColBatch>` in each node's private data.
unsafe fn export_json_array(batch: &Arc<ColBatch>, column: &JsonColumn, out: *mut ArrowArray) {
    let length = column.len() as i64;
    let null_count = column.null_count() as i64;
    let validity = json_validity_buffer(column);
    match column.body() {
        JsonBody::Text(text) => {
            let mut buffers = vec![validity];
            push_offsets(&mut buffers, &text.offsets, EMPTY_OFFSETS_I32.as_ptr());
            buffers.push(text.data.as_ptr() as *const c_void);
            write_array_node(
                batch,
                out,
                length,
                null_count,
                buffers,
                Vec::new(),
                ptr::null_mut(),
            );
        }
        JsonBody::Structured(structured) => {
            let mut children = Vec::new();
            for (_, values) in &structured.typed {
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                export_one_column(batch, values, child);
                children.push(child);
            }
            for (_, dynamic) in &structured.dynamic {
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                export_dynamic_array(batch, dynamic, child);
                children.push(child);
            }
            let shared = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_json_shared_array(batch, structured, shared);
            children.push(shared);
            write_array_node(
                batch,
                out,
                length,
                null_count,
                vec![validity],
                children,
                ptr::null_mut(),
            );
        }
    }
}

/// Export a structured JSON body's `_shared_data` as an Arrow LargeList of a
/// (`paths`, `values`) struct. `shared_offsets` is already the i64 list-offset
/// run and both string columns are borrowed zero-copy; `paths` is utf8 and
/// `values` is opaque binary, but they share one physical layout (validity,
/// i32 offsets, data) so `export_binary_array` builds both. The list length is
/// the JSON row count; the inner struct length is the flattened pair count.
unsafe fn export_json_shared_array(
    batch: &Arc<ColBatch>,
    structured: &StructuredJson,
    out: *mut ArrowArray,
) {
    let paths = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    export_binary_array(batch, &structured.shared_paths, paths);
    let values = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    export_binary_array(batch, &structured.shared_values, values);
    let item = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    write_array_node(
        batch,
        item,
        structured.shared_paths.len() as i64,
        0,
        vec![ptr::null()],
        vec![paths, values],
        ptr::null_mut(),
    );
    let mut buffers = vec![ptr::null()];
    push_offsets(
        &mut buffers,
        &structured.shared_offsets,
        EMPTY_OFFSETS_I64.as_ptr(),
    );
    write_array_node(
        batch,
        out,
        structured.len as i64,
        0,
        buffers,
        vec![item],
        ptr::null_mut(),
    );
}

/// Per-JSON-column cache of the synthetic routing buffers every absent
/// dynamic path shares: the 0..len dense-union offsets run and one all-`id`
/// type-ids run per distinct NULL-child id. Dense-union semantics fix both
/// runs exactly for an all-NULL child, so sharing them across paths is
/// byte-correct and avoids one `5 * len`-byte allocation per absent path.
struct NullUnionBuffers {
    len: usize,
    offsets: Option<Arc<Vec<i32>>>,
    type_ids: Vec<(i8, Arc<Vec<i8>>)>,
}

impl NullUnionBuffers {
    fn new(len: usize) -> Self {
        Self {
            len,
            offsets: None,
            type_ids: Vec::new(),
        }
    }

    /// The shared 0..len offsets run, built on first use.
    fn offsets(&mut self) -> Arc<Vec<i32>> {
        Arc::clone(self.offsets.get_or_insert_with(|| {
            Arc::new((0..self.len).map(|row| row as i32).collect::<Vec<i32>>())
        }))
    }

    /// The shared all-`id` type-ids run for one NULL-child id, built on first
    /// use per distinct id.
    fn type_ids(&mut self, id: i8) -> Arc<Vec<i8>> {
        if let Some((_, ids)) = self.type_ids.iter().find(|(known, _)| *known == id) {
            return Arc::clone(ids);
        }
        let ids = Arc::new(vec![id; self.len]);
        self.type_ids.push((id, Arc::clone(&ids)));
        ids
    }
}

/// Export an all-NULL Dynamic union of `buffers.len` rows against a result-wide
/// plan. Used when a JSON dynamic path is absent from one chunk: every row
/// routes to the union's trailing NULL child and the non-null children are
/// empty.
///
/// Unlike [`export_dynamic_array_with_plan`], every buffer is OWNED by the
/// exported node (the type ids and the dense-union offsets are Arc-shared
/// through the node's [`ArrayPrivateData`], see [`NullUnionBuffers`]), so
/// nothing borrows from a caller-local column. A synthetic `DynamicColumn`
/// routed through the borrowing fast path would hand out a dangling offsets
/// pointer, because only the `Arc<ColBatch>` is kept alive in private data, not
/// the synthetic column. The output is byte-identical to what the borrowing
/// path would produce for an all-NULL block, just backed by owned storage. Real
/// (batch-owned) columns keep the zero-copy borrow.
unsafe fn export_null_dynamic_with_plan(
    batch: &Arc<ColBatch>,
    plan: &DynamicExportPlan,
    buffers: &mut NullUnionBuffers,
    chunk: usize,
    out: *mut ArrowArray,
) {
    let len = buffers.len;
    // The NULL child is a Nothing column of `len` rows; each row's dense-union
    // offset is its occurrence index there, i.e. the 0..len run.
    let null_offsets = buffers.offsets();
    let null_child = |batch: &Arc<ColBatch>| {
        let out = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
        export_one_column(batch, &Column::Nothing(NothingColumn::new(len)), out);
        out
    };

    if plan.children.len() < ARROW_UNION_MAX_CHILDREN {
        let null_id = plan.children.len() as i8;
        let type_ids = buffers.type_ids(null_id);
        let mut children = (0..plan.children.len())
            .map(|global| {
                export_dynamic_plan_child_array(batch, &plan.children[global], None, chunk)
            })
            .collect::<Vec<_>>();
        children.push(null_child(batch));
        write_shared_union_array_node(batch, out, len, type_ids, null_offsets, children);
        return;
    }

    // 128+ planned children: every row routes to the trailing null group, so
    // each non-null group node is empty and the outer node's shared ids all
    // name the null group.
    let num_groups = plan.children.len().div_ceil(ARROW_UNION_MAX_CHILDREN);
    let outer_ids = buffers.type_ids(num_groups as i8);
    let mut children = Vec::with_capacity(num_groups + 1);
    for group in 0..num_groups {
        let first = group * ARROW_UNION_MAX_CHILDREN;
        let end = (first + ARROW_UNION_MAX_CHILDREN).min(plan.children.len());
        let group_children = (first..end)
            .map(|global| {
                export_dynamic_plan_child_array(batch, &plan.children[global], None, chunk)
            })
            .collect::<Vec<_>>();
        let group_array = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
        write_owned_union_array_node(
            batch,
            group_array,
            0,
            Vec::new(),
            ptr::null(),
            Vec::new(),
            group_children,
        );
        children.push(group_array);
    }
    children.push(null_child(batch));
    write_shared_union_array_node(batch, out, len, outer_ids, null_offsets, children);
}

unsafe fn export_dynamic_null_array(
    batch: &Arc<ColBatch>,
    dynamic: &DynamicColumn,
) -> *mut ArrowArray {
    let out = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    let null_column = Column::Nothing(dynamic.nulls.clone());
    export_one_column(batch, &null_column, out);
    out
}

/// Export one non-null inner union group of a 128+ alternative Variant.
unsafe fn export_variant_group_array(
    batch: &Arc<ColBatch>,
    variant: &VariantColumn,
    group: &VariantGroup,
    out: *mut ArrowArray,
) {
    let buffers = vec![
        group.type_ids.as_ptr() as *const c_void,
        group.offsets.as_ptr() as *const c_void,
    ];
    let end = group
        .first_variant
        .saturating_add(ARROW_UNION_MAX_CHILDREN)
        .min(variant.variants.len());
    let child_columns = variant
        .variants
        .get(group.first_variant..end)
        .unwrap_or_default();
    let mut children = Vec::with_capacity(child_columns.len());
    for child_column in child_columns {
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
        export_one_column(batch, child_column, child);
        children.push(child);
    }
    write_array_node(
        batch,
        out,
        group.type_ids.len() as i64,
        0,
        buffers,
        children,
        ptr::null_mut(),
    );
}

/// Export Variant's intrinsic NULL rows as the final Arrow Null child.
unsafe fn export_variant_null_array(
    batch: &Arc<ColBatch>,
    variant: &VariantColumn,
    out: *mut ArrowArray,
) {
    // Arrow Null has no borrowed buffers, so this temporary wrapper can be
    // dropped after `export_one_column` copies its length into ArrowArray.
    let null_column = Column::Nothing(variant.nulls.clone());
    export_one_column(batch, &null_column, out);
}

/// Finish one Arrow array node and transfer its buffer/child ownership to the
/// existing release private data.
unsafe fn write_array_node(
    batch: &Arc<ColBatch>,
    out: *mut ArrowArray,
    length: i64,
    null_count: i64,
    buffers: Vec<*const c_void>,
    children: Vec<*mut ArrowArray>,
    dictionary: *mut ArrowArray,
) {
    let n_children = children.len() as i64;
    let pd = Box::new(ArrayPrivateData {
        buffers,
        children,
        _owned_type_ids: Vec::new(),
        _owned_offsets: Vec::new(),
        _shared_type_ids: None,
        _shared_offsets: None,
        _batch: Arc::clone(batch),
        dictionary,
    });

    let array = &mut *out;
    array.length = length;
    array.null_count = null_count;
    array.offset = 0;
    array.n_buffers = pd.buffers.len() as i64;
    // Moving the Box into raw ownership below does not move either Vec's heap
    // allocation. These pointer arrays therefore stay stable until release.
    array.buffers = if pd.buffers.is_empty() {
        ptr::null_mut()
    } else {
        pd.buffers.as_ptr() as *mut *const c_void
    };
    array.n_children = n_children;
    array.children = if pd.children.is_empty() {
        ptr::null_mut()
    } else {
        pd.children.as_ptr() as *mut *mut ArrowArray
    };
    array.dictionary = pd.dictionary;
    array.release = Some(release_array);
    array.private_data = Box::into_raw(pd) as *mut c_void;
}

/// Finish a synthesized Dynamic union node. `type_ids` is always owned here.
/// Flat Dynamic borrows its existing i32 offsets through `borrowed_offsets`;
/// nested outer/group nodes pass newly synthesized `owned_offsets` instead.
unsafe fn write_owned_union_array_node(
    batch: &Arc<ColBatch>,
    out: *mut ArrowArray,
    length: usize,
    type_ids: Vec<i8>,
    borrowed_offsets: *const i32,
    owned_offsets: Vec<i32>,
    children: Vec<*mut ArrowArray>,
) {
    let mut pd = Box::new(ArrayPrivateData {
        buffers: Vec::with_capacity(2),
        children,
        _owned_type_ids: type_ids,
        _owned_offsets: owned_offsets,
        _shared_type_ids: None,
        _shared_offsets: None,
        _batch: Arc::clone(batch),
        dictionary: ptr::null_mut(),
    });
    let offsets = if length == 0 {
        ptr::null()
    } else if pd._owned_offsets.is_empty() {
        borrowed_offsets
    } else {
        pd._owned_offsets.as_ptr()
    };
    let type_ids = if length == 0 {
        ptr::null()
    } else {
        pd._owned_type_ids.as_ptr()
    };
    pd.buffers.push(type_ids as *const c_void);
    pd.buffers.push(offsets as *const c_void);

    let array = &mut *out;
    array.length = length as i64;
    array.null_count = 0;
    array.offset = 0;
    array.n_buffers = 2;
    array.buffers = pd.buffers.as_ptr() as *mut *const c_void;
    array.n_children = pd.children.len() as i64;
    array.children = if pd.children.is_empty() {
        ptr::null_mut()
    } else {
        pd.children.as_ptr() as *mut *mut ArrowArray
    };
    array.dictionary = ptr::null_mut();
    array.release = Some(release_array);
    array.private_data = Box::into_raw(pd) as *mut c_void;
}

/// Finish a synthetic all-NULL Dynamic union node whose type-ids and offsets
/// buffers are Arc-shared across nodes (see [`NullUnionBuffers`]). The Vec heap
/// behind each Arc never moves, so the pointers taken here stay stable until
/// the release callback drops this node's Arc clones.
unsafe fn write_shared_union_array_node(
    batch: &Arc<ColBatch>,
    out: *mut ArrowArray,
    length: usize,
    type_ids: Arc<Vec<i8>>,
    offsets: Arc<Vec<i32>>,
    children: Vec<*mut ArrowArray>,
) {
    let (ids_ptr, offsets_ptr) = if length == 0 {
        (ptr::null(), ptr::null())
    } else {
        (
            type_ids.as_ptr() as *const c_void,
            offsets.as_ptr() as *const c_void,
        )
    };
    let pd = Box::new(ArrayPrivateData {
        buffers: vec![ids_ptr, offsets_ptr],
        children,
        _owned_type_ids: Vec::new(),
        _owned_offsets: Vec::new(),
        _shared_type_ids: Some(type_ids),
        _shared_offsets: Some(offsets),
        _batch: Arc::clone(batch),
        dictionary: ptr::null_mut(),
    });

    let array = &mut *out;
    array.length = length as i64;
    array.null_count = 0;
    array.offset = 0;
    array.n_buffers = 2;
    array.buffers = pd.buffers.as_ptr() as *mut *const c_void;
    array.n_children = pd.children.len() as i64;
    array.children = if pd.children.is_empty() {
        ptr::null_mut()
    } else {
        pd.children.as_ptr() as *mut *mut ArrowArray
    };
    array.dictionary = ptr::null_mut();
    array.release = Some(release_array);
    array.private_data = Box::into_raw(pd) as *mut c_void;
}

fn push_primitive_buffers<T>(
    buffers: &mut Vec<*const c_void>,
    values: &[T],
    validity: &Option<crate::bitmap::Bitmap>,
) {
    match validity {
        Some(bm) => buffers.push(bm.as_bytes().as_ptr() as *const c_void),
        None => buffers.push(ptr::null()),
    }
    buffers.push(values.as_ptr() as *const c_void);
}

/// A single zero offset: the offsets buffer for a length-0 variable-length array
/// whose column carries an empty `offsets` `Vec`.
///
/// The Arrow C Data Interface requires the offsets buffer of a variable-size
/// binary or list array to hold `length + 1` elements starting with 0 (Columnar
/// format spec, "Variable-size Binary Layout"). For a length-0 array that is
/// exactly one element, so the buffer's byte size is nonzero and its pointer MAY
/// NOT be null: the C Data Interface allows a null buffer pointer only when the
/// buffer's byte size would be 0 (see ArrowArray.buffers). A hand-built column
/// can leave `offsets` empty (a zero-capacity `Vec` whose `as_ptr()` is a
/// dangling pointer with no leading 0); the decoder always emits `[0]`, but
/// `Column` fields are public. Exporting these shared 'static single zeros keeps
/// the exported buffer spec-compliant and interoperable with strict consumers
/// (pyarrow, arrow-rs) instead of handing out a dangling pointer. A 'static
/// outlives every consumer and is never freed by a release callback, which frees
/// only the boxed private data and releases child arrays.
static EMPTY_OFFSETS_I32: [i32; 1] = [0];
static EMPTY_OFFSETS_I64: [i64; 1] = [0];

/// Push a variable-length column's offsets buffer, substituting a 'static single
/// zero when `offsets` is empty so the export never hands out the dangling
/// `as_ptr()` of a zero-capacity `Vec` and always satisfies Arrow's `length + 1`
/// offsets contract (see [`EMPTY_OFFSETS_I32`] / [`EMPTY_OFFSETS_I64`]).
fn push_offsets<T>(buffers: &mut Vec<*const c_void>, offsets: &[T], empty: *const T) {
    let ptr = if offsets.is_empty() {
        empty
    } else {
        offsets.as_ptr()
    };
    buffers.push(ptr as *const c_void);
}

/// Returns an [`ExportError`] without touching `out` when a Dynamic column's
/// block-local child set exceeds [`DYNAMIC_MAX_EXPORT_CHILDREN`] or carries
/// duplicate child type names, so it must be paired with the matching
/// (also-checked) [`export_batch_schema`], preferably through [`export_batch`],
/// which checks once for both.
///
/// # Safety
///
/// `out` must be a valid, writable pointer to an `ArrowArray`, normally a
/// zeroed struct. The exported array borrows the batch buffers and keeps the
/// `Arc<ColBatch>` alive until `out` is freed through its `release` callback.
/// If the batch schema contains Dynamic or JSON, pair this only with
/// [`export_batch_schema`], preferably through [`export_batch`]; the logical
/// [`export_schema`] API cannot describe Dynamic's block-local children or
/// JSON's block-local dynamic paths and body-kind choice, so pairing this array
/// with an [`export_schema`] schema yields a mismatched schema/array pair.
///
/// Every Dynamic column's routing buffers (`type_ids` indexing `children`,
/// `u32::MAX` for NULL) must be internally consistent, as the decoder and
/// encode validation produce them. A column whose public buffers were mutated
/// into an inconsistent state exports as memory-safe but semantically invalid
/// Arrow: a row whose id indexes no child is routed to the NULL child while
/// its dense-union offset still names the slot the id originally selected
/// (see the fallback in [`export_dynamic_array`]).
pub unsafe fn export_batch_array(
    batch: &Arc<ColBatch>,
    out: *mut ArrowArray,
) -> Result<(), ExportError> {
    check_dynamic_export(batch)?;
    write_batch_array(batch, out);
    Ok(())
}

/// Infallible body of [`export_batch_array`]; callers must have already run
/// [`check_dynamic_export`] on `batch`.
unsafe fn write_batch_array(batch: &Arc<ColBatch>, out: *mut ArrowArray) {
    let num_rows = batch.num_rows as i64;
    let n_children = batch.num_columns() as i64;

    let mut child_arrays: Vec<*mut ArrowArray> = Vec::with_capacity(n_children as usize);
    for i in 0..batch.num_columns() {
        // Safety: an all-zero `ArrowArray` is a valid initial value, same as in
        // `export_schema`: pointers null, integers zero, `Option<extern fn>` the
        // all-zero `None` niche. `export_column_array` overwrites every field.
        let child_array = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
        export_column_array(batch, i, child_array);
        child_arrays.push(child_array);
    }

    let pd = Box::new(ArrayPrivateData {
        buffers: vec![ptr::null()],
        children: child_arrays.clone(),
        _owned_type_ids: Vec::new(),
        _owned_offsets: Vec::new(),
        _shared_type_ids: None,
        _shared_offsets: None,
        _batch: Arc::clone(batch),
        dictionary: ptr::null_mut(),
    });

    let array = &mut *out;
    array.length = num_rows;
    array.null_count = 0;
    array.offset = 0;
    array.n_buffers = 1;
    array.buffers = pd.buffers.as_ptr() as *mut *const c_void;
    array.n_children = n_children;
    array.children = pd.children.as_ptr() as *mut *mut ArrowArray;
    array.dictionary = ptr::null_mut();
    array.release = Some(release_array);
    array.private_data = Box::into_raw(pd) as *mut c_void;
}

unsafe fn export_batch_array_with_plans(
    batch: &Arc<ColBatch>,
    plans: &[FieldExportPlan],
    chunk: usize,
    out: *mut ArrowArray,
) {
    let mut children = Vec::with_capacity(batch.num_columns());
    for (column, plan) in batch.columns.iter().zip(plans) {
        let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
        export_one_column_with_plan(batch, column, plan, chunk, child);
        children.push(child);
    }
    write_array_node(
        batch,
        out,
        batch.num_rows as i64,
        0,
        vec![ptr::null()],
        children,
        ptr::null_mut(),
    );
}

unsafe fn export_one_column_with_plan(
    batch: &Arc<ColBatch>,
    column: &Column,
    plan: &FieldExportPlan,
    chunk: usize,
    out: *mut ArrowArray,
) {
    match (plan, column) {
        (FieldExportPlan::Array(item_plan), Column::Array(column)) => {
            let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_one_column_with_plan(batch, column.values.as_ref(), item_plan, chunk, child);
            let mut buffers = vec![ptr::null()];
            push_offsets(&mut buffers, &column.offsets, EMPTY_OFFSETS_I64.as_ptr());
            write_array_node(
                batch,
                out,
                column.len() as i64,
                0,
                buffers,
                vec![child],
                ptr::null_mut(),
            );
        }
        (FieldExportPlan::Tuple(element_plans), Column::Tuple(column)) => {
            let mut children = Vec::with_capacity(column.fields.len());
            for (field, child_plan) in column.fields.iter().zip(element_plans) {
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                export_one_column_with_plan(batch, field, child_plan, chunk, child);
                children.push(child);
            }
            let validity = column.validity.as_ref().map_or(ptr::null(), |bitmap| {
                bitmap.as_bytes().as_ptr() as *const c_void
            });
            write_array_node(
                batch,
                out,
                column.len as i64,
                column.null_count() as i64,
                vec![validity],
                children,
                ptr::null_mut(),
            );
        }
        (FieldExportPlan::Map { key, value }, Column::Map(column)) => {
            let entries = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_map_entries_with_plan(
                batch,
                column.entries.as_ref(),
                key,
                value,
                chunk,
                entries,
            );
            let mut buffers = vec![ptr::null()];
            push_offsets(&mut buffers, &column.offsets, EMPTY_OFFSETS_I64.as_ptr());
            write_array_node(
                batch,
                out,
                column.len() as i64,
                0,
                buffers,
                vec![entries],
                ptr::null_mut(),
            );
        }
        (FieldExportPlan::Variant(child_plans), Column::Variant(column)) => {
            export_variant_array_with_plans(batch, column, child_plans, chunk, out);
        }
        (FieldExportPlan::Dynamic(dynamic_plan), Column::Dynamic(column)) => {
            export_dynamic_array_with_plan(batch, column, dynamic_plan, chunk, out);
        }
        (FieldExportPlan::Json(json_plan), Column::Json(column)) => {
            export_json_array_with_plan(batch, column, json_plan, chunk, out);
        }
        _ => export_one_column(batch, column, out),
    }
}

/// Export one `JSON` column against its result-wide stream plan. Typed paths
/// recurse per plan; dynamic paths are the unified result-wide set, each routed
/// to this chunk's matching Dynamic column or, when the chunk lacks the path, to
/// a synthetic all-NULL union of the block's row count; `_shared_data` is plain
/// utf8/binary and reuses the standalone writer.
unsafe fn export_json_array_with_plan(
    batch: &Arc<ColBatch>,
    column: &JsonColumn,
    plan: &JsonExportPlan,
    chunk: usize,
    out: *mut ArrowArray,
) {
    let length = column.len() as i64;
    let null_count = column.null_count() as i64;
    let validity = json_validity_buffer(column);
    match (plan, column.body()) {
        (JsonExportPlan::Text, JsonBody::Text(text)) => {
            let mut buffers = vec![validity];
            push_offsets(&mut buffers, &text.offsets, EMPTY_OFFSETS_I32.as_ptr());
            buffers.push(text.data.as_ptr() as *const c_void);
            write_array_node(
                batch,
                out,
                length,
                null_count,
                buffers,
                Vec::new(),
                ptr::null_mut(),
            );
        }
        (JsonExportPlan::Structured { typed, dynamic, .. }, JsonBody::Structured(structured)) => {
            let mut children = Vec::new();
            for (path_plan, (_, values)) in typed.iter().zip(&structured.typed) {
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                export_one_column_with_plan(batch, values, path_plan, chunk, child);
                children.push(child);
            }
            // Map this chunk's block-local dynamic paths by name so a result-wide
            // path present here reuses its column and an absent one exports an
            // all-NULL union of the block's length.
            let mut present: std::collections::HashMap<&str, &DynamicColumn> =
                std::collections::HashMap::with_capacity(structured.dynamic.len());
            for (path, dynamic) in &structured.dynamic {
                present.insert(path.as_str(), dynamic);
            }
            // One chunk-local buffer cache shared by every absent path below.
            let mut null_buffers = NullUnionBuffers::new(structured.len);
            for (path, path_plan) in dynamic {
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                match present.get(path.as_str()) {
                    Some(dynamic) => {
                        export_dynamic_array_with_plan(batch, dynamic, path_plan, chunk, child)
                    }
                    // Absent path: export an all-NULL union of the block's row
                    // count with OWNED buffers, so no offsets pointer dangles
                    // into a caller-local column.
                    None => export_null_dynamic_with_plan(
                        batch,
                        path_plan,
                        &mut null_buffers,
                        chunk,
                        child,
                    ),
                }
                children.push(child);
            }
            let shared = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
            export_json_shared_array(batch, structured, shared);
            children.push(shared);
            write_array_node(
                batch,
                out,
                length,
                null_count,
                vec![validity],
                children,
                ptr::null_mut(),
            );
        }
        // A zero-row block carries no JSON state prefix, so its body kind is
        // whatever `empty_column` built; export the planned kind's empty shape
        // instead (no buffer of the mismatched body is borrowed).
        _ if column.is_empty() => {
            let typed_paths: &[(String, ChType)] = match plan {
                JsonExportPlan::Text => &[],
                JsonExportPlan::Structured { typed_paths, .. } => typed_paths,
            };
            export_empty_json_with_plan(batch, typed_paths, plan, chunk, out);
        }
        // Unreachable for a well-formed stream: the plan body kind is derived
        // from these same chunks, so a Structured plan pairs only with
        // Structured bodies and a Text plan only with Text (a nonempty mix is
        // rejected as JsonBodyKindMismatch before the first batch). A body
        // mutated after the plan was fixed lands here; fall back to the
        // memory-safe block-local shape even though it may disagree with the
        // fixed stream schema.
        _ => {
            debug_assert!(
                false,
                "JSON body kind disagrees with its stream export plan"
            );
            export_json_array(batch, column, out);
        }
    }
}

unsafe fn export_map_entries_with_plan(
    batch: &Arc<ColBatch>,
    entries: &Column,
    key_plan: &FieldExportPlan,
    value_plan: &FieldExportPlan,
    chunk: usize,
    out: *mut ArrowArray,
) {
    let Column::Tuple(entries) = entries else {
        export_one_column(batch, entries, out);
        return;
    };
    let mut children = Vec::with_capacity(2);
    if let [keys, values] = entries.fields.as_slice() {
        let key = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
        export_one_column_with_plan(batch, keys, key_plan, chunk, key);
        children.push(key);
        let value = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
        export_one_column_with_plan(batch, values, value_plan, chunk, value);
        children.push(value);
    }
    write_array_node(
        batch,
        out,
        entries.len as i64,
        0,
        vec![ptr::null()],
        children,
        ptr::null_mut(),
    );
}

unsafe fn export_variant_array_with_plans(
    batch: &Arc<ColBatch>,
    variant: &VariantColumn,
    plans: &[FieldExportPlan],
    chunk: usize,
    out: *mut ArrowArray,
) {
    let mut children = Vec::new();
    let buffers = match &variant.layout {
        VariantLayout::Flat { type_ids, offsets } => {
            for (column, plan) in variant.variants.iter().zip(plans) {
                let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                export_one_column_with_plan(batch, column, plan, chunk, child);
                children.push(child);
            }
            vec![
                type_ids.as_ptr() as *const c_void,
                offsets.as_ptr() as *const c_void,
            ]
        }
        VariantLayout::Nested {
            type_ids,
            offsets,
            groups,
        } => {
            for (group_index, group) in groups.iter().enumerate() {
                let first = group_index * ARROW_UNION_MAX_CHILDREN;
                let end = (first + ARROW_UNION_MAX_CHILDREN).min(variant.variants.len());
                let mut group_children = Vec::with_capacity(end - first);
                for (column, plan) in variant.variants[first..end].iter().zip(&plans[first..end]) {
                    let child = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                    export_one_column_with_plan(batch, column, plan, chunk, child);
                    group_children.push(child);
                }
                let group_array = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
                write_array_node(
                    batch,
                    group_array,
                    group.type_ids.len() as i64,
                    0,
                    vec![
                        group.type_ids.as_ptr() as *const c_void,
                        group.offsets.as_ptr() as *const c_void,
                    ],
                    group_children,
                    ptr::null_mut(),
                );
                children.push(group_array);
            }
            vec![
                type_ids.as_ptr() as *const c_void,
                offsets.as_ptr() as *const c_void,
            ]
        }
    };
    let null = Box::into_raw(Box::new(std::mem::zeroed::<ArrowArray>()));
    let null_column = Column::Nothing(variant.nulls.clone());
    export_one_column(batch, &null_column, null);
    children.push(null);
    write_array_node(
        batch,
        out,
        variant.len() as i64,
        0,
        buffers,
        children,
        ptr::null_mut(),
    );
}

// ---------------------------------------------------------------------------
// Stream export
// ---------------------------------------------------------------------------

/// errno-compatible code the stream callbacks return when the stream failed to
/// initialize (see [`StreamPrivateData::init_error`]). The Arrow C Stream
/// contract asks for a nonzero errno-style code on error; `EINVAL` marks the
/// supplied result as unexportable.
const STREAM_INIT_ERROR: i32 = 22; // EINVAL

unsafe extern "C" fn stream_get_schema(
    stream: *mut ArrowArrayStream,
    out: *mut ArrowSchema,
) -> i32 {
    let s = &mut *stream;
    let pd = &*(s.private_data as *const StreamPrivateData);
    if pd.init_error {
        // Do not emit a schema the per-chunk arrays could never match. Clear the
        // consumer's release slot so nothing half-written is left behind; the
        // reason is available through `get_last_error`.
        (*out).release = None;
        return STREAM_INIT_ERROR;
    }
    export_schema_with_plans(&pd.schema, &pd.plans, out);
    0
}

unsafe extern "C" fn stream_get_next(stream: *mut ArrowArrayStream, out: *mut ArrowArray) -> i32 {
    let s = &mut *stream;
    let pd = &mut *(s.private_data as *mut StreamPrivateData);
    if pd.init_error {
        (*out).release = None;
        return STREAM_INIT_ERROR;
    }
    match pd.chunks.next() {
        Some((chunk, batch)) => {
            export_batch_array_with_plans(&batch, &pd.plans, chunk, out);
            0
        }
        None => {
            // End of stream: mark the array released-and-empty per the spec.
            let a = &mut *out;
            a.release = None;
            0
        }
    }
}

unsafe extern "C" fn stream_get_last_error(stream: *mut ArrowArrayStream) -> *const c_char {
    let s = &*stream;
    let pd = &*(s.private_data as *const StreamPrivateData);
    pd.error_msg.as_ptr()
}

/// Export a sequence of chunks as a single Arrow C Stream — one record batch
/// per chunk. `schema` is used for `get_schema` and remains valid even when
/// `chunks` is empty (a zero-row result still advertises its columns).
///
/// A result-wide Dynamic child set can outgrow Arrow's signed Int8 union code
/// space (see [`DYNAMIC_MAX_EXPORT_CHILDREN`]), and a hand-built Dynamic column
/// can carry duplicate block-local child type names, which would make the
/// name-keyed result-wide unification ambiguous. In either case the stream is
/// still constructed but flagged failed: `get_schema` and `get_next` return a
/// nonzero code and `get_last_error` reports the reason, rather than emitting a
/// schema and arrays that would disagree.
///
/// # Safety
///
/// `out` must be a valid, writable pointer to an `ArrowArrayStream`, normally a
/// zeroed struct. On return `out` owns its data and must be freed through its
/// `release` callback per the Arrow C Data Interface.
///
/// Every exported Dynamic column's routing buffers (`type_ids` indexing
/// `children`, `u32::MAX` for NULL) must be internally consistent, as the
/// decoder and encode validation produce them. A column whose public buffers
/// were mutated into an inconsistent state exports as memory-safe but
/// semantically invalid Arrow: an unresolved id is routed to the NULL child
/// while its borrowed dense-union offset still indexes the child it originally
/// named (see the fallback in [`export_dynamic_array_with_plan`]).
pub unsafe fn export_chunks_to_stream(
    schema: Schema,
    chunks: Vec<Arc<ColBatch>>,
    out: *mut ArrowArrayStream,
) {
    let num_chunks = chunks.len();
    let mut plans: Vec<FieldExportPlan> = Vec::with_capacity(schema.num_fields());
    let mut error: Option<ExportError> = None;
    for (index, field) in schema.fields.iter().enumerate() {
        let columns = chunks
            .iter()
            .enumerate()
            .filter_map(|(chunk, batch)| batch.columns.get(index).map(|column| (chunk, column)))
            .collect::<Vec<_>>();
        match build_export_plan(&field.ch_type, &columns, num_chunks) {
            Ok(plan) => plans.push(plan),
            Err(err) => {
                error = Some(err);
                break;
            }
        }
    }

    // Refuse the whole stream up front when any Dynamic node exceeds the union
    // code space rather than let the schema tree and array tree disagree. The
    // message reuses the same `ExportError` the standalone batch path returns.
    // The column-side walk backs up the plan checks for parity with the
    // standalone guard: a hand-built column whose Dynamic hides under a node
    // the plan collapses to `Plain` (e.g. an illegal LowCardinality(Dynamic))
    // is invisible to the plan walk but still descended by the
    // `export_one_column` fallback.
    if error.is_none() {
        error = plans
            .iter()
            .find_map(dynamic_plan_over_limit)
            .map(|children| ExportError::DynamicUnionTooWide {
                children,
                limit: DYNAMIC_MAX_EXPORT_CHILDREN,
            });
    }
    if error.is_none() {
        error = chunks
            .iter()
            .flat_map(|chunk| &chunk.columns)
            .find_map(|column| check_column_dynamic_export(column).err());
    }
    let (init_error, error_msg) = match error {
        Some(err) => (true, cstring_lossy(&err.to_string())),
        None => (false, cstring_lossy("")),
    };

    let pd = Box::new(StreamPrivateData {
        schema,
        plans,
        chunks: chunks.into_iter().enumerate(),
        init_error,
        error_msg,
    });

    let s = &mut *out;
    s.get_schema = Some(stream_get_schema);
    s.get_next = Some(stream_get_next);
    s.get_last_error = Some(stream_get_last_error);
    s.release = Some(release_stream);
    s.private_data = Box::into_raw(pd) as *mut c_void;
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests;
