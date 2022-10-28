# TW Lightning Proto, Wallet-core/LDK
Based on ldk-sample
https://github.com/lightningdevkit/ldk-sample


### Details

- Proto is based on `ldk-sample`, https://github.com/lightningdevkit/ldk-sample
- It uses:
  - wallet-core (link with it), for address derivation and funding transaction preparation/signing, and
  - a Btc Core node (bitcoind), external, for obtaining UTXOs and as a block source
- It simulates a rudimentary wallet: imported from mnemonic, single address used, private key stored.
- At channel open, funding tx is created, from wallet address, using wallet-core
- When funds are no longer used by LDK, and become spendable, transfers them to L1 wallet (main address)
- It has setting for default LN peer (first-hop or routing node)
- It has `opendc` (open default connection) to connect to default peer
- LDK state is stored in LDK data dir (as in original sample app)


### For building

- Prerequisite: `wallet-core` project folder, with sources and built binaries, min `3.0.9`.
- Prerequisite: Rust (`rustc`, `cargo`)
- Set wallet-core project folder in `src/build.rs`

```
static WALLET_CORE_PROJECT_DIR: &str = "../../../../wallet-core";
```

- Build:

```
cargo build
```

### For running

- Settings are stored in `.env`, set it up, see `env_sample`
- First wallet has to be imported, by mnemonic, use `importwallet` argument.  Private key is stored in `.pk_secret` unencrypted. Wallet should have funds.
```
cargo run importwallet
```

- Used Bitcoin Core node has to have the wallet address loaded.
- Run app
- Open channel to another node, command `openchannel`.
- Pay an invoice, command `sendpayment`.
- Close channel (`closechannel`).


### TODO

- Auto channel open on send
- Get rid of Btc Core for UTXOs, use Blockbook
- Get rid of Btc Core for block source, use Blockbook
- Get rid of Btc Core: get network info, fee, broadcast
- (Move wallet-core proj dir setting from build.rs to env)
- (check in rust interfacing Rust module in wallet core, use it from there)




## Installation
```
git clone https://github.com/lightningdevkit/ldk-sample
```

## Usage
```
cd ldk-sample
cargo run <bitcoind-rpc-username>:<bitcoind-rpc-password>@<bitcoind-rpc-host>:<bitcoind-rpc-port> <ldk_storage_directory_path> [<ldk-peer-listening-port>] [bitcoin-network] [announced-listen-addr announced-node-name]
```
`bitcoind`'s RPC username and password likely can be found through `cat ~/.bitcoin/.cookie`.

`bitcoin-network`: defaults to `testnet`. Options: `testnet`, `regtest`, and `signet`.

`ldk-peer-listening-port`: defaults to 9735.

`announced-listen-addr` and `announced-node-name`: default to nothing, disabling any public announcements of this node.
`announced-listen-addr` can be set to an IPv4 or IPv6 address to announce that as a publicly-connectable address for this node.
`announced-node-name` can be any string up to 32 bytes in length, representing this node's alias.

## License

Licensed under either:

 * Apache License, Version 2.0 ([LICENSE-APACHE](LICENSE-APACHE) or http://www.apache.org/licenses/LICENSE-2.0)
 * MIT License ([LICENSE-MIT](LICENSE-MIT) or http://opensource.org/licenses/MIT)

at your option.
