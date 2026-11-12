//! Which functions and pragmas an agent read may use (SECURITY.md layer B, "Functions").
//!
//! The rule is an **allow-list**: a function is allowed only when it is on [`allowed`] for the
//! engine (or the human marked it safe in [`crate::Policy::safe_functions`]). The deny-list
//! ([`denied`]) is checked first and wins over everything, the human's safe list included; it
//! exists to give a precise message and so that a mistake in the allow-list cannot let a known
//! side-effect function through.
//!
//! Names are compared after normalisation: unquoted names are lower-cased (both engines fold
//! them); a quoted PostgreSQL name keeps its case, so `"NEXTVAL"` is a different (unknown, so
//! denied) function. A schema-qualified name is allowed only as `pg_catalog.<allowed>`.

use quarry_core::EngineKind;

/// Function name prefixes that are never allowed in an agent read, whatever the policy says.
/// Lower-case; a name matches when it starts with an entry ending in `_` or `*`-style families
/// listed here, or equals an entry.
const DENIED_EXACT: &[&str] = &[
    // sequences
    "nextval",
    "setval",
    "lastval",
    // sessions and server control
    "pg_terminate_backend",
    "pg_cancel_backend",
    "pg_reload_conf",
    "pg_rotate_logfile",
    "pg_promote",
    "pg_switch_wal",
    "pg_create_restore_point",
    "pg_log_backend_memory_contexts",
    "pg_notify",
    "pg_import_system_collations",
    "pg_truncate_visibility_map",
    "set_config",
    "setseed",
    // files on the server
    "pg_read_file",
    "pg_read_binary_file",
    "pg_stat_file",
    "pg_file_write",
    "pg_file_rename",
    "pg_file_unlink",
    "pg_file_sync",
    // transactions
    "pg_current_xact_id",
    "pg_current_xact_id_if_assigned",
    "pg_export_snapshot",
    // run a query given as text (can call anything)
    "query_to_xml",
    "query_to_xmlschema",
    "query_to_xml_and_xmlschema",
    "table_to_xml",
    "table_to_xmlschema",
    "table_to_xml_and_xmlschema",
    "cursor_to_xml",
    "cursor_to_xmlschema",
    "schema_to_xml",
    "schema_to_xmlschema",
    "schema_to_xml_and_xmlschema",
    "database_to_xml",
    "database_to_xmlschema",
    "database_to_xml_and_xmlschema",
    "ts_stat",
    // SQLite
    "load_extension",
    "fts3_tokenizer",
    "writefile",
    "readfile",
    "edit",
];

/// Denied name families (prefixes).
const DENIED_PREFIX: &[&str] = &[
    "pg_advisory_",
    "pg_try_advisory_",
    "pg_ls_",
    "pg_stat_reset",
    "pg_backup_",
    "pg_start_backup",
    "pg_stop_backup",
    "pg_wal_replay_",
    "pg_create_",
    "pg_drop_replication_slot",
    "pg_copy_",
    "pg_logical_",
    "pg_replication_",
    "pg_sleep",
    "lo_",
    "dblink",
    "txid_",
    "binary_upgrade_",
];

/// Functions that are refused even in a proposal: they change the connection itself.
const NEVER: &[&str] = &["load_extension"];

/// Pure or read-only functions shared by both engines (aggregates, windows, scalars).
const COMMON: &[&str] = &[
    // aggregates
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "total",
    "group_concat",
    "string_agg",
    // windows
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
    "nth_value",
    // conditionals
    "coalesce",
    "nullif",
    "greatest",
    "least",
    "ifnull",
    "iif",
    // strings
    "lower",
    "upper",
    "length",
    "char_length",
    "character_length",
    "octet_length",
    "substr",
    "substring",
    "trim",
    "ltrim",
    "rtrim",
    "replace",
    "instr",
    "concat",
    "concat_ws",
    "format",
    "printf",
    "hex",
    "unhex",
    "quote",
    "char",
    "unicode",
    "reverse",
    "left",
    "right",
    "repeat",
    "lpad",
    "rpad",
    // numbers
    "abs",
    "round",
    "ceil",
    "ceiling",
    "floor",
    "trunc",
    "sign",
    "sqrt",
    "power",
    "pow",
    "exp",
    "ln",
    "log",
    "log10",
    "log2",
    "mod",
    "pi",
    "degrees",
    "radians",
    "sin",
    "cos",
    "tan",
    "asin",
    "acos",
    "atan",
    "atan2",
    "random",
    // dates
    "date",
    "time",
    "now",
    "current_date",
    "current_time",
    "current_timestamp",
    "localtime",
    "localtimestamp",
    // json
    "json_array_length",
    "json_extract_path",
    "json_extract_path_text",
    "json_object",
    "json_array",
    "json_each",
    "json_typeof",
    "json_valid",
];

/// PostgreSQL additions. Every one reads or computes; none writes, sleeps, touches files or
/// sessions.
const POSTGRES: &[&str] = &[
    // aggregates and statistics
    "array_agg",
    "json_agg",
    "jsonb_agg",
    "json_object_agg",
    "jsonb_object_agg",
    "bool_and",
    "bool_or",
    "every",
    "bit_and",
    "bit_or",
    "bit_xor",
    "stddev",
    "stddev_pop",
    "stddev_samp",
    "variance",
    "var_pop",
    "var_samp",
    "percentile_cont",
    "percentile_disc",
    "mode",
    "corr",
    "covar_pop",
    "covar_samp",
    "regr_slope",
    "regr_intercept",
    "regr_count",
    "regr_r2",
    "regr_avgx",
    "regr_avgy",
    "any_value",
    // strings
    "btrim",
    "position",
    "strpos",
    "split_part",
    "initcap",
    "md5",
    "sha224",
    "sha256",
    "sha384",
    "sha512",
    "encode",
    "decode",
    "to_hex",
    "ascii",
    "chr",
    "regexp_replace",
    "regexp_match",
    "regexp_matches",
    "regexp_split_to_array",
    "regexp_split_to_table",
    "regexp_like",
    "regexp_count",
    "regexp_instr",
    "regexp_substr",
    "starts_with",
    "translate",
    "quote_ident",
    "quote_literal",
    "quote_nullable",
    "overlay",
    "string_to_array",
    "array_to_string",
    "bit_length",
    "convert_from",
    "convert_to",
    "normalize",
    "similar_to_escape",
    // numbers
    "cbrt",
    "div",
    "width_bucket",
    "gcd",
    "lcm",
    "factorial",
    "scale",
    "trim_scale",
    "min_scale",
    // dates and times
    "date_trunc",
    "date_part",
    "date_bin",
    "age",
    "to_char",
    "to_date",
    "to_timestamp",
    "to_number",
    "make_date",
    "make_time",
    "make_timestamp",
    "make_timestamptz",
    "make_interval",
    "justify_days",
    "justify_hours",
    "justify_interval",
    "isfinite",
    "clock_timestamp",
    "statement_timestamp",
    "transaction_timestamp",
    "timeofday",
    "timezone",
    "extract",
    // json / jsonb (pure: they build or read values, never tables)
    "to_json",
    "to_jsonb",
    "row_to_json",
    "array_to_json",
    "json_build_object",
    "jsonb_build_object",
    "json_build_array",
    "jsonb_build_array",
    "jsonb_extract_path",
    "jsonb_extract_path_text",
    "jsonb_array_length",
    "jsonb_each",
    "json_each_text",
    "jsonb_each_text",
    "json_array_elements",
    "jsonb_array_elements",
    "json_array_elements_text",
    "jsonb_array_elements_text",
    "json_object_keys",
    "jsonb_object_keys",
    "jsonb_typeof",
    "jsonb_pretty",
    "jsonb_set",
    "jsonb_set_lax",
    "jsonb_insert",
    "jsonb_strip_nulls",
    "json_strip_nulls",
    "jsonb_path_query",
    "jsonb_path_exists",
    "jsonb_path_match",
    "jsonb_path_query_array",
    "jsonb_path_query_first",
    "json_populate_record",
    "jsonb_populate_record",
    "json_to_record",
    "jsonb_to_record",
    "json_to_recordset",
    "jsonb_to_recordset",
    // arrays and sets
    "array_length",
    "array_upper",
    "array_lower",
    "array_ndims",
    "array_dims",
    "cardinality",
    "unnest",
    "array_position",
    "array_positions",
    "array_remove",
    "array_replace",
    "array_append",
    "array_prepend",
    "array_cat",
    "array_fill",
    "generate_series",
    "generate_subscripts",
    "trim_array",
    // text search
    "to_tsvector",
    "to_tsquery",
    "plainto_tsquery",
    "phraseto_tsquery",
    "websearch_to_tsquery",
    "ts_rank",
    "ts_rank_cd",
    "ts_headline",
    "setweight",
    "strip",
    "numnode",
    "querytree",
    // uuids and misc values
    "gen_random_uuid",
    "uuidv4",
    "uuidv7",
    "num_nulls",
    "num_nonnulls",
    // session information (reads)
    "current_user",
    "session_user",
    "user",
    "current_role",
    "current_schema",
    "current_schemas",
    "current_database",
    "current_catalog",
    "current_setting",
    "version",
    "inet_client_addr",
    "pg_backend_pid",
    "pg_postmaster_start_time",
    "pg_conf_load_time",
    "pg_is_in_recovery",
    // catalog introspection (reads)
    "pg_typeof",
    "format_type",
    "col_description",
    "obj_description",
    "shobj_description",
    "pg_get_viewdef",
    "pg_get_indexdef",
    "pg_get_constraintdef",
    "pg_get_triggerdef",
    "pg_get_functiondef",
    "pg_get_function_arguments",
    "pg_get_function_result",
    "pg_get_expr",
    "pg_get_userbyid",
    "pg_get_serial_sequence",
    "pg_get_keywords",
    "pg_table_size",
    "pg_relation_size",
    "pg_total_relation_size",
    "pg_indexes_size",
    "pg_database_size",
    "pg_column_size",
    "pg_size_pretty",
    "pg_size_bytes",
    "pg_table_is_visible",
    "pg_type_is_visible",
    "pg_function_is_visible",
    "pg_relation_filenode",
    "has_table_privilege",
    "has_schema_privilege",
    "has_column_privilege",
    "has_database_privilege",
    "has_function_privilege",
    "has_sequence_privilege",
    "has_any_column_privilege",
    "pg_has_role",
    "to_regclass",
    "to_regtype",
    "to_regproc",
    "to_regnamespace",
    "to_regrole",
    "pg_encoding_to_char",
    "pg_char_to_encoding",
    "pg_column_is_updatable",
    "pg_relation_is_updatable",
    "pg_options_to_table",
    "pg_get_partkeydef",
    "pg_partition_tree",
    "pg_partition_root",
    "pg_partition_ancestors",
    // casts written as calls
    "text",
    "int2",
    "int4",
    "int8",
    "float4",
    "float8",
    "numeric",
    "bool",
    "timestamp",
    "timestamptz",
    "interval",
    "varchar",
    "bpchar",
];

/// SQLite additions.
const SQLITE: &[&str] = &[
    "glob",
    "like",
    "likelihood",
    "likely",
    "unlikely",
    "typeof",
    "zeroblob",
    "randomblob",
    "datetime",
    "julianday",
    "unixepoch",
    "strftime",
    "timediff",
    "sqlite_version",
    "soundex",
    "median",
    "percentile",
    // json
    "json",
    "jsonb",
    "json_extract",
    "jsonb_extract",
    "json_insert",
    "jsonb_insert",
    "json_patch",
    "jsonb_patch",
    "json_remove",
    "jsonb_remove",
    "json_replace",
    "jsonb_replace",
    "json_set",
    "jsonb_set",
    "json_type",
    "json_quote",
    "json_group_array",
    "jsonb_group_array",
    "json_group_object",
    "jsonb_group_object",
    "json_tree",
    "json_pretty",
    "json_error_position",
    "jsonb_object",
    "jsonb_array",
    // pragma table-valued functions (SQLite only exposes pragmas without side effects this way)
    "pragma_table_info",
    "pragma_table_xinfo",
    "pragma_table_list",
    "pragma_index_list",
    "pragma_index_info",
    "pragma_index_xinfo",
    "pragma_foreign_key_list",
    "pragma_database_list",
    "pragma_collation_list",
    "pragma_function_list",
    "pragma_pragma_list",
];

/// SQLite pragmas that only read and take no argument. With `= value` or `(value)` they would set
/// something, so an argument is refused.
const PRAGMA_NO_ARG: &[&str] = &[
    "application_id",
    "collation_list",
    "compile_options",
    "data_version",
    "database_list",
    "encoding",
    "freelist_count",
    "function_list",
    "module_list",
    "page_count",
    "page_size",
    "pragma_list",
    "schema_version",
    "table_list",
    "user_version",
];

/// SQLite pragmas that only read and take a table / index / schema name as their argument
/// (`PRAGMA table_info(t)`); `PRAGMA x = v` is still refused.
const PRAGMA_NAME_ARG: &[&str] = &[
    "table_info",
    "table_xinfo",
    "table_list",
    "index_list",
    "index_info",
    "index_xinfo",
    "foreign_key_list",
    "foreign_key_check",
    "integrity_check",
    "quick_check",
];

/// Whether `name` (normalised, unqualified) is a known side-effect function.
pub fn denied(name: &str) -> bool {
    let lower = name.to_lowercase();
    DENIED_EXACT.contains(&lower.as_str()) || DENIED_PREFIX.iter().any(|p| lower.starts_with(p))
}

/// Whether `name` must be refused even in a proposal.
pub fn never(name: &str) -> bool {
    NEVER.contains(&name.to_lowercase().as_str())
}

/// Whether `name` (normalised, unqualified) is on the engine's read-only allow-list.
pub fn allowed(name: &str, engine: EngineKind) -> bool {
    COMMON.contains(&name)
        || match engine {
            EngineKind::Postgres => POSTGRES.contains(&name),
            EngineKind::Sqlite => SQLITE.contains(&name),
        }
}

/// What a pragma may take, if it is a read.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PragmaArg {
    None,
    Name,
}

/// The read-only pragma `name` (lower-cased), or `None` when it is not on the list.
pub fn read_pragma(name: &str) -> Option<PragmaArg> {
    // `table_list` is on both lists: `PRAGMA table_list` and `PRAGMA table_list(t)` both read.
    if PRAGMA_NAME_ARG.contains(&name) {
        Some(PragmaArg::Name)
    } else if PRAGMA_NO_ARG.contains(&name) {
        Some(PragmaArg::None)
    } else {
        None
    }
}

/// The whole allow-list for an engine, for documentation and tests.
pub fn allow_list(engine: EngineKind) -> Vec<&'static str> {
    let extra = match engine {
        EngineKind::Postgres => POSTGRES,
        EngineKind::Sqlite => SQLITE,
    };
    COMMON.iter().chain(extra.iter()).copied().collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_allowed_function_is_also_denied() {
        for engine in [EngineKind::Postgres, EngineKind::Sqlite] {
            for f in allow_list(engine) {
                assert!(!denied(f), "{f} is on both lists for {engine:?}");
                assert_eq!(f, f.to_lowercase(), "allow-list entries are lower-case");
            }
        }
    }
}
