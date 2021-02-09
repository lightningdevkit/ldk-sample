The LDK sample is broken between 2 primary process : `ldk-node` and `ldk-cli`.

The `ldk-cli` is a simple JSON-RPC client to interact with the node.

The `ldk-node` is a multi-threaded process embedding all the main components of a regular Lightning
node :
* `MicroSPVClient`, a chain validation backend providing blocks updates, utxo filtering and utxo set access
* `ChainMonitor`, a chain processing entity enforcing onchain the channel logic negotiated offchain
* `EventHandler`, a default handler informing the client logic about internal events
* `ChannelManager`, a core lightning state machine, supervising channel lifecycle and payment initiation/relay
* `NetGraphMsgHandler`, a lightning router, handling gossips updates and providing payment path draws
* `PeerManager`, a lightning networking stack, managing peer connections lifecycle
* `setup_socket_listener`, a simple network handler to listen incoming connections
* `setup_rpc_server`, a simple RPC server handler to listen to `ldk-cli`

All those components are running in their own threads and interact through inter-threading
handlers powered by `tokio::sync::mpsc`.

# A Gentle Guided Tour of LDK-sample components

The initial step is to load state from previous node session and user config file. Note the sample
config file LdkConfig is wider than RL's UserConfig as it scopes daemon-specific configuration 
variables (`bitcoind` RPC port, bitcoin network, ...)

```
        let daemon_dir = if env::args().len() > 2 {
                let path_dir = env::args().skip(2).next().unwrap();
                let path = if fs::metadata(&path_dir).unwrap().is_dir() {
                        path_dir + "/ldk-node.conf"
                } else { exit_on!("Need daemon directory to exist and be a directory"); };
                path
        } else {
                let mut path = if let Some(path) = env::home_dir() {
                        path
                } else { exit_on!("No home directory found"); };
                path.push(".ldk-node/ldk-node.conf");
                String::from_str(path.to_str().unwrap()).unwrap()
        };

        let ldk_config = LdkConfig::create(&daemon_dir);
```

The second step is establishing a connection with your Bitcoin validation backend. It should 
always be recalled that your Lightning node must keep a lively view of the chain to ensure
security and well-accuracy of its operations. Note, you should also have permanent access to
the tx-relay p2p network and a trustworhty fee-estimation. For the sample, we opted to rely on
Bitcoin Core RPC interface, but other options such as Electrum or BIP157 can be envisioned.

```
        let bitcoind_client = Arc::new(RpcClient::new(&ldk_config.get_str("bitcoind_credentials"), &ldk_config.get_str("bitcoind_hostport")));

        let network = if let Ok(net) = ldk_config.get_network() {
                net
        } else { exit_on!("No network found in config file"); };

        log_sample!(logger, "Checking validity of RPC URL to bitcoind...");
        if let Ok(v) = bitcoind_client.make_rpc_call("getblockchaininfo", &[], false).await {
                assert!(v["verificationprogress"].as_f64().unwrap() > 0.99);
                assert!(
                        v["bip9_softforks"]["segwit"]["status"].as_str() == Some("active") ||
                        v["softforks"]["segwit"]["type"].as_str() == Some("buried"));
                let bitcoind_net = match v["chain"].as_str().unwrap() {
                        "main" => constants::Network::Bitcoin,
                        "test" => constants::Network::Testnet,
                        "regtest" => constants::Network::Regtest,
                        _ => panic!("Unknown network type"),
                };
                if !(network == bitcoind_net) { exit_on!("Divergent network between LDK node and bitcoind"); }
        } else { exit_on!("Failed to connect to bitcoind RPC server, check your `bitcoind_hostport`/`bitcoind_credentials` settings"); }

```

The third step is the initialization or loading of the LN-specific key material. Ideally this material
should be encrypted and replicated to increase security and fault-tolerance of your node. We're
using the default LDK key-management sample `KeysManager`).

```
        let secp_ctx = Secp256k1::new();

        let our_node_seed = if let Ok(seed) = fs::read(data_path.clone() + "/key_seed") {
                assert_eq!(seed.len(), 32);
                let mut key = [0; 32];
                key.copy_from_slice(&seed);
                key
        } else {
                let mut key = [0; 32];
                thread_rng().fill_bytes(&mut key);
                let mut f = fs::File::create(data_path.clone() + "/key_seed").unwrap();
                f.write_all(&key).expect("Failed to write seed to disk");
                f.sync_all().expect("Failed to sync seed to disk");
                key
        };
        let cur = SystemTime::now().duration_since(SystemTime::UNIX_EPOCH).unwrap();
        let keys = Arc::new(KeysManager::new(&our_node_seed, network, cur.as_secs(), cur.subsec_nanos()));
```

We previously established our connection through RPC with a Bitcoin Core. In the fourth step,
we'll use this RPC access to boostrap our chain backend client `MicroSPVClient`. This entity
is providing block connections/disconnections through the `ChainListener` interface 
(implem `ChainConnector`, a subcomponent of `MicroSPVClient`).

Current consumers of those chain updates are `ChannelManager`/`ChainMonitor`/`NetworkGraphHandler`.
Requirements of these components will be detailed in further steps.

```
        log_sample!(logger, "Starting chain backend thread...");

        let (outbound_blocks_chan_manager, inbound_blocks_chan_manager) = mpsc::channel(100);

        let buffer_blocks = Arc::new(ChainConnector::new());
        let chain_listener = buffer_blocks.clone();

        let (handle_1, handle_2) = setup_chain_backend(starting_blockhash.unwrap(), (ldk_config.get_str("bitcoind_credentials"), ldk_config.get_str("bitcoind_hostport")), buffer_blocks, chain_listener, outbound_blocks_chan_manager).await;
        join_handles.push(handle_1);
        join_handles.push(handle_2);
```

The fifth step is starting the chain processing thread. Onchain events to care about are numerous
and spread from revoked counterparty commitment to punish to monitoring channel outputs belonging to
us until they're reorg-safe to spend. The LDK sample is relying on the default RL implementation
`ChainMonitor`. This component relies on `MicroSPVClient` through the `chain::Filter` to register
UTXO to watch, `BroadcasterInterface` to propagate "reactive" transactions and `FeeEstimator` to
vet those transactions with a good feerate.

This component is critical for the safety of the Lightning node and its state should be dutifully
persisted and replicated. It will serves the `ChannelManager` as a `chain::Watch` interface. 
TODO: code a connector linking ChainMonitor to ChannelManager.

```
        log_sample!(logger, "Starting chain processing...");

        let chain_source = Arc::new(ChainSource::new());
        let handle = setup_chain_processing(chain_source.clone(), tx_broadcaster.clone(), fee_estimator.clone(), logger.clone(), persister.clone()).await;
        join_handles.push(handle);
```

The sixth step is the warm up of an event handler thread. Current LDK sample handler is pretty
summary, and only covers notification of inbound connections. It might serve as a default endpoint
to propopagate `Events` to the client logic (e.g payment reception, peer disconnections, ...).

It receives updates from the peer manager thread through the `InboundEventConnector`.

```
        log_sample!(logger, "Starting event handler...");

        let (outbound_event_peers, inbound_event_peers) = mpsc::channel(1);
        let (outbound_event_chan_manager, inbound_event_chan_manager) = mpsc::channel(1);

        let inbound_event_connector = InboundEventConnector::new(inbound_event_peers, inbound_event_chan_manager);

        let handle = setup_event_handler(inbound_event_connector).await;
        join_handles.push(handle);
```

The seventh step is the most meaningful one of the initialization sequence, setting up `ChannelManager`.
This component is driving per-channels logics, receiving updates, dispatching them, relaying HTLCs,
initiating payments or receiving ones. It will consumes block updates from the connector `ChainConnector`,
produced by `MicroSPVClient`. For per-channel key material, it requires access to a `KeysInterface` 
(implem `KeysManager`).

The LDK `ChannelManager thread is waiting RPC command on the `rpccmd` communication channel. Those
commands like `open` or `send` will trigger corresponding state updates in the channel targeted.
`MessageSendEvent` might be generated and send to the network thread through the `chanman_msg_events`/
`peerman_notify` communications channels.

Interactions with the network thread are bidirectional, which means that `ChannelManager` will also
consume network messages from the `netmsg` communication channel. Those messages are issued by a 
connected peer and will also affect channel states.

It's also consuming network messages through the `netmsg` communication channel. Those messages are
issued by a connected peer and will also affect channel states and might trigger response back
to the network thread.

```
        log_sample!(logger, "Starting channel manager...");

        let (outbound_chanman_rpccmd, inbound_chanman_rpccmd) = mpsc::channel(1);
        let (outbound_chanman_netmsg, inbound_chanman_netmsg) = mpsc::channel(1);
        let (outbound_chanman_msg_events, inbound_chanman_msg_events) = mpsc::channel(1);
        let (outbound_peerman_notify, inbound_peerman_notify) = mpsc::channel(1);

        let outbound_chan_manager_connector = OutboundChanManagerConnector::new(outbound_chanman_msg_events, outbound_peerman_notify);

        let chain_watchdog = Arc::new(ChainWatchdog::new());
        let (handle_1, handle_2) = setup_channel_manager(inbound_chanman_rpccmd, inbound_chanman_netmsg, inbound_blocks_chan_manager, outbound_chan_manager_connector, network, fee_estimator.clone(), chain_watchdog.clone(), tx_broadcaster.clone(), logger.clone(), keys.clone(), config, 0, bitcoind_client.clone()).await;
        join_handles.push(handle_1);
        join_handles.push(handle_2);
```

To send payment, our `ChannelManager` requires access to the graph. LDK sample gossips handling
and routing processing is done by RL's default implementation `NetGraphMsgHandler`. This component
is dependent of `MicroSPVClient` to fetch and verify UTXO of announced channels. UTXO access
is provided through the `chain::Access` interface. TODO: actually do the communication channel for
this access.

```
        log_sample!(logger, "Starting node router...");

        let (outbound_router_rpccmd, inbound_router_rpccmd) = mpsc::channel(1);
        let (outbound_router_rpcreply, inbound_router_rpcreply) = mpsc::channel(1);
        let (outbound_routing_msg, inbound_routing_msg): (Sender<Event>, Receiver<Event>) = mpsc::channel(1);

        let utxo_accessor = Arc::new(UtxoWatchdog::new());
        let outbound_router_connector = OutboundRouterConnector::new(outbound_router_rpcreply);
        let inbound_router_connector = InboundRouterConnector::new(inbound_router_rpccmd, inbound_router_rpcreply);
        let handle = setup_router(outbound_router_connector, inbound_router_connector, utxo_accessor.clone(), logger.clone()).await;
        join_handles.push(handle);
```

The ninth step is concerning the LN network stack, `PeerManager`. This thread handles network
encryption and messages traffic with Lightning peers. It's interacting with the RPC server through
the `peers_rpccmd` communication channel for network-related commands such as `connect`/`disconnect`.
It's also both in a role of producer/consumers w.rt to `ChannelManager`, relaying channel updates
messages (`update_add_htlc`, `commitment_signed`, ...) and consuming back `MessageSendEvent`. Note,
it will be also triggered by the inbound connection thread through the `new_inbound_connection` 
callback.

```
        log_sample!(logger, "Starting peer manager...");

        let (outbound_peers_rpccmd, inbound_peers_rpccmd) = mpsc::channel(1);
        let (outbound_socket_events, inbound_socket_events): (Sender<()>, Receiver<()>) = mpsc::channel(1);

        let outbound_peer_manager_connector = Arc::new(OutboundPeerManagerConnector::new(outbound_event_peers)); //sender_routing_msg/sender_chan_msg
        let buffer_netmsg = Arc::new(BufferNetMsg::new(inbound_chanman_msg_events));
        let chan_handler = buffer_netmsg.clone();
```

The tenth step is setuping a small inbound connection thread, serving as an endpoint for any
Lightning network connections initiated by our peers. It's servicing on the configured LN port
from `LdkConfig` and will delay any furhter peer management processing to the corresponding `PeerManager`
thread.

```
        log_sample!(logger, "Starting socket listener thread...");

        let outbound_socket_listener_connector = OutboundSocketListenerConnector::new(outbound_socket_events); // outbound_socket_events
        let handle = setup_socket_listener(peer_manager_arc, outbound_socket_listener_connector, ln_port).await;
        join_handles.push(handle);
```
The last step to complete the initialization sequence is setting up a RPC server. Servicing the
`ldk-cli` binary, it will dispatch all available commands to the others components and route back
their results. Binding port is configurable through `LdkConfig`.

```
        log_sample!(logger, "Starting rpc server thread...");

        let outbound_rpc_server_connector = OutboundRPCServerConnector::new(outbound_peers_rpccmd, outbound_chanman_rpccmd, outbound_router_rpccmd);
        let handles = setup_rpc_server(outbound_rpc_server_connector, ldk_port).await;
        join_handles.push(handles);
```

