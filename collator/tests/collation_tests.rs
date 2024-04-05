use tycho_collator::{
    mempool::{MempoolAdapterBuilder, MempoolAdapterBuilderStdImpl, MempoolAdapterStdImpl},
    state_node::{StateNodeAdapterBuilder, StateNodeAdapterBuilderStdImpl, StateNodeAdapterStdImpl},
    test_utils::try_init_test_tracing,
    types::CollationConfig,
    validator_test_impl::ValidatorProcessorTestImpl,
};

#[tokio::test]
async fn test_collation_process_on_stubs() {
    try_init_test_tracing(tracing_subscriber::filter::LevelFilter::TRACE);

    let config = CollationConfig {
        key_pair: everscale_crypto::ed25519::KeyPair::generate(&mut rand::thread_rng()),
        mc_block_min_interval_ms: 5000,
    };
    let mpool_adapter_builder = MempoolAdapterBuilderStdImpl::<MempoolAdapterStdImpl>::new();
    let state_node_adapter_builder = StateNodeAdapterBuilderStdImpl::<StateNodeAdapterStdImpl>::new();

    tracing::info!("Trying to start CollationManager");

    let node_network = tycho_collator::test_utils::create_node_network();

    let _manager = tycho_collator::manager::create_std_manager_with_validator::<
        _,
        _,
        ValidatorProcessorTestImpl<_>,
    >(config, mpool_adapter_builder, state_node_adapter_builder, node_network);

    tokio::select! {
        _ = tokio::signal::ctrl_c() => {
            println!();
            println!("Ctrl-C received, shutting down the test");
        }
    }
}
