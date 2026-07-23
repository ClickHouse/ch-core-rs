//! Wire-protocol constants shared by decode and encode.
//!
//! Only these constants live here; the decode and encode logic itself lives in
//! `decode/` and `encode/`.

/// Server protocol revision this crate has been validated against
/// (ClickHouse v26.6.1.1193-stable). Pass this as `DecodeOptions::protocol_revision`
/// when decoding a Native stream produced by a current server over the native
/// TCP protocol.
pub const DBMS_TCP_PROTOCOL_VERSION: u64 = 54485;

/// Protocol revision at which every column header carries a one-byte
/// custom-serialization marker before its data (server constant
/// `DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION`).
pub const DBMS_MIN_REVISION_WITH_CUSTOM_SERIALIZATION: u64 = 54454;

/// Protocol revision at which a data block's `BlockInfo` carries the
/// `out_of_order_buckets` field (server
/// `DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS_IN_AGGREGATION` in
/// `src/Core/ProtocolDefines.h`). At or above it, `BlockInfo::write` emits field 3
/// with an empty vector for a plain data block, which `read_block_info` consumes
/// and [`super::encode`] re-emits, so both stay byte-identical to the server writer.
pub(crate) const DBMS_MIN_REVISION_WITH_OUT_OF_ORDER_BUCKETS: u64 = 54480;

/// Protocol revision at which NativeWriter switches Dynamic and JSON from the
/// legacy V1 structure prefix to V2. V2 removes V1's ignored legacy count; the
/// value body is otherwise unchanged.
pub(crate) const DBMS_MIN_REVISION_WITH_V2_DYNAMIC_AND_JSON_SERIALIZATION: u64 = 54473;

/// Maximum wrapper/container nesting depth the type-name parser accepts.
///
/// The type string is attacker-controlled wire input, and every wrapper level
/// (`Nullable`, `LowCardinality`, `Array`, `Tuple`, `Map`) recurses one
/// stack frame in `parse_ch_type_depth`. An unbounded string like
/// `Array(Array(...Array(Int32)...))` would overflow the stack and abort the
/// process (SIGABRT is uncatchable), violating the "malformed bytes return an
/// error, never crash" invariant. Capping the PARSER caps every downstream
/// recursion too: decode, the completeness scan, and the Arrow export only recurse
/// as deep as the parsed `ChType`, so a rejected over-deep header never reaches
/// them. 100 far exceeds any real ClickHouse schema (real nested types are a
/// handful of levels deep) and stays safe even on small worker-thread stacks.
/// ClickHouse's own analogous guard is `max_parser_depth` (default 1000).
/// `pub(crate)` so the encoder's `validate_column` can enforce the same cap on
/// caller-constructed types, which never pass through this parser.
pub(crate) const MAX_TYPE_DEPTH: usize = 100;

/// Key serialization version this decoder accepts. The Native format always
/// uses `SharedDictionariesWithAdditionalKeys` (server
/// `KeysSerializationVersion`).
pub(crate) const LOW_CARDINALITY_KEY_VERSION: u64 = 1;

/// `NeedGlobalDictionaryBit` of the per-block index type word. Native never sets
/// it (the server rejects it for `native_format`), so the decoder rejects it too.
pub(crate) const LC_NEED_GLOBAL_DICTIONARY_BIT: u64 = 1 << 8;
/// `HasAdditionalKeysBit` of the per-block index type word. Always set in Native:
/// each block carries its own dictionary as "additional keys".
pub(crate) const LC_HAS_ADDITIONAL_KEYS_BIT: u64 = 1 << 9;
/// `NeedUpdateDictionary` of the per-block index type word. Native writes it
/// alongside `HasAdditionalKeysBit` for the per-block dictionary. The decoder
/// does not require it so older or synthetic payloads with only
/// `HasAdditionalKeysBit` still decode.
pub(crate) const LC_NEED_UPDATE_DICTIONARY_BIT: u64 = 1 << 10;
