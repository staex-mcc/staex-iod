# Provisioner

## Usage

```shell
cargo run -- --help
```

### Example config file

Config file should be location in provisioner root folder for default file path value.

```shell
cargo run -- config
```

```toml
log_level = "DEBUG"
rpc_url = "wss://rpcpc1-qa.agung.peaq.network"

[signer]
typ = "SecretUri"
val = "//Alice"

[faucet]
amount = 999993264201726756

[faucet.signer]
typ = "SecretUri"
val = "//Alice"

[device]
sync = true
force = false

[device.attributes]
data_type = "cctv-camera"
location = "40.1949288120072,44.55177253802097"
price_access = 42.03995
price_pin = 445.12222

[device.attributes.additional]
microcontroller = "stm32"
device_age_in_years = 2

[indexer]
from_block = 1731233
dsn = "sqlite:staex-iod.sqlite"
host = "127.0.0.1"
port = 4698
```

## Get devices by HTTP API

```shell
curl -s -X GET -G 'http://127.0.0.1:4698/devices?limit=10&offset=0' --data-urlencode 'filters[0][field]=data_type' --data-urlencode 'filters[0][condition]==' --data-urlencode 'filters[0][value]=cctv-camera' | jq
```

```json
[
  {
    "address": "5CwQRPkqmUg5arWuJtw2qoGRL4oRDjguzmrcrSSsv35Cuv3s",
    "version": "v1",
    "device": {
      "data_type": "cctv-camera",
      "location": "40.1949288120072,44.55177253802097",
      "price_pin": "445.12222",
      "price_access": "42.03995"
    },
    "updated_at": 1707386161
  }
]
```

## Force device on-chain update

Currently provisioner doesn't compare additional fields from config device with on-chain device additional fields while sync. So to update additional fields you need to enable force sync.
