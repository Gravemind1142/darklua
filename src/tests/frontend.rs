#[test]
fn deserialize_treat_indexing_as_noopt_flag() {
    let config: darklua_core::Configuration =
        json5::from_str("{ treat_indexing_as_noopt: true }").unwrap();
    assert!(config.treat_indexing_as_noopt);
    // round-trip
    let serialized = json5::to_string(&config).unwrap();
    assert!(serialized.contains("treat_indexing_as_noopt"));
} 