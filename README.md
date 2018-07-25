Simple Sample rust-lightning-based Lightning Node

* Uses Bitcoin Core's RPC interface for non-channel funds management as well as consensus data.
* Accepts commands on the command line to perform Lightning actions.
* panic!()s if you try to use this on mainnet as most data is not persisted to disk and error handling is generally a crapshoot.
* Assumes you have a local copy of rust-lightning and rust-lightning-invoice from the rust-bitcoin project in the same directory as this repo.

* Can connect to nodes/accept incoming connections.
* Can open outbound channels and receive inbound channels.
* Can send payments over multiple hops using in-built router and BOLT11 parsing from rust-lightning-invoice (which is not yet complete, so you have to repeat the final node's node_id on the command line).
* Can receive payments but cannot yet generate BOLT11 invoices.
