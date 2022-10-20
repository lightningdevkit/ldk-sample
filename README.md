# ldk-sample
Sample node implementation using LDK.

## Trust Wallet Specific

### For building

- Add wallet-core project folder with built binaries to `src/build.rs`

```
static WALLET_CORE_PROJECT_DIR: &str = "../../../wallet-core"
```

### For running

- Settings are stored in `.env`, see `env_sample`
- Wallet private key is stored in `.pk_secret`, can be imported using `importwallet` option
- LDK state is stored in LDK data dir (as in original sample app)

### TODO

- Check with Btc node that wallet address is loaded (listunspent or getaddressinfo/ismine)
- Testnet/derivation support in AnyAddress (walletcore)
- Move wallet-core proj dir from build.rs to env
- checkin rust interfacing Rust module in wallet core, use it from there






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
