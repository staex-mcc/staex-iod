use std::fmt::{Debug, Display};
use std::str::{from_utf8, FromStr};

use clap::{Parser, Subcommand};
use contract_transcode::{ContractMessageTranscoder, Value};
use log::{debug, info, trace, warn, Level};
use pallet_contracts_primitives::ContractExecResult;
use scale::{DecodeAll, Encode};
use subxt::backend::legacy::LegacyRpcMethods;
use subxt::backend::rpc::RpcClient;
use subxt::error::{RpcError, TransactionError};
use subxt::events::Events;
use subxt::tx::{Signer, TxPayload, TxStatus};
use subxt::utils::MultiAddress;
use subxt::{
    backend::rpc, config::substrate::H256, rpc_params, utils::AccountId32, OnlineClient,
    PolkadotConfig,
};
use subxt_signer::sr25519::Keypair;
use subxt_signer::SecretUri;

use crate::did::api::runtime_apis::contracts_api::types::Call;
use crate::did::api::runtime_types::contracts_node_runtime::RuntimeEvent;
use crate::did::api::runtime_types::frame_system::EventRecord;
use crate::did::api::runtime_types::pallet_balances::pallet::Event as BalancesEvent;
use crate::did::api::runtime_types::pallet_contracts::pallet::Event as ContractsEvent;
use crate::did::api::TransactionApi;

mod did;

type SUBXTConfig = PolkadotConfig;

type Error = Box<dyn std::error::Error>;

#[derive(Debug)]
#[allow(clippy::upper_case_acronyms)]
enum CoinType {
    Planck,
    DOT,
}

#[derive(Debug)]
struct Balance {
    raw: u128,
    typ: CoinType,
}

impl Display for Balance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let postfix = match self.typ {
            CoinType::Planck => "Planck",
            CoinType::DOT => "DOT",
        };
        write!(f, "{} {}", self.raw, postfix)
    }
}

impl Balance {
    fn from_planck(val: u128) -> Self {
        Self {
            raw: val,
            typ: CoinType::Planck,
        }
    }

    fn from_dot(val: u128) -> Self {
        Self {
            raw: val,
            typ: CoinType::DOT,
        }
    }

    fn as_planck(&self) -> Self {
        let raw = match self.typ {
            CoinType::Planck => self.raw,
            CoinType::DOT => self.raw * 10u128.pow(10),
        };
        Self {
            raw,
            typ: CoinType::Planck,
        }
    }

    fn as_dot(&self) -> Self {
        let raw = match self.typ {
            CoinType::Planck => self.raw / 10u128.pow(10),
            CoinType::DOT => self.raw,
        };
        Self {
            raw,
            typ: CoinType::DOT,
        }
    }
}

#[repr(u8)]
enum DIDEvent {
    BeforeFlipping = 0,
    AfterFlipping = 1,
}

impl DIDEvent {
    fn from(raw: u8) -> DIDEvent {
        match raw {
            0 => DIDEvent::BeforeFlipping,
            1 => DIDEvent::AfterFlipping,
            _ => unimplemented!(),
        }
    }
}

#[derive(scale::Decode, Debug)]
struct BeforeFlipping {
    from: AccountId32,
    field1: u64,
    field2: String,
    field3: String,
}

impl Display for BeforeFlipping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {:?} / {:?} / {:?}", self.from, self.field1, self.field2, self.field3)
    }
}

#[derive(scale::Decode, Debug)]
struct AfterFlipping {
    from: AccountId32,
    field1: u64,
    field2: String,
    field3: bool,
}

impl Display for AfterFlipping {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {:?} / {:?} / {:?}", self.from, self.field1, self.field2, self.field3)
    }
}

#[derive(scale::Encode, scale::Decode, Debug)]
struct Weight {
    #[codec(compact)]
    ref_time: u64,
    #[codec(compact)]
    proof_size: u64,
}

impl Weight {
    fn new(ref_time: u64, proof_size: u64) -> Self {
        Self {
            ref_time,
            proof_size,
        }
    }
}

impl From<Weight> for crate::did::api::runtime_types::sp_weights::weight_v2::Weight {
    fn from(value: Weight) -> Self {
        Self {
            ref_time: value.ref_time,
            proof_size: value.proof_size,
        }
    }
}

#[derive(Debug)]
struct DryRunResult {
    data: Value,
    gas_required: Weight,
}

impl DryRunResult {
    fn to_get_message_res(&self) -> Result<bool, Error> {
        match &self.data {
            Value::Tuple(t) => {
                if t.values().count() != 1 {
                    return Err(format!("unexpected values count: {}", t.values().count()).into());
                }
                let value = t.values().last().ok_or::<&str>("last value is not found")?;
                match value {
                    Value::Bool(b) => Ok(*b),
                    _ => Err("unexpected response: value in tuple is not bool".into()),
                }
            }
            _ => Err("unexpected response: value is not tuple".into()),
        }
    }
}

/// Command line utility to interact with StaexIoD provisioner.
#[derive(Parser)]
#[clap(name = "provisioner")]
#[command(author, version, about, long_about = None)]
struct Cli {
    /// Provisioner config file path.
    #[arg(short, long)]
    #[arg(default_value = "config.toml")]
    config: String,
    #[clap(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Show default config for provisioner.
    Config {},
    /// Run provisioner.
    Run {},
    /// Create new account.
    NewAccount {
        /// Faucet account on creation.
        /// This flag requires node to be online.
        #[arg(short, long)]
        #[arg(default_value = "false")]
        faucet: bool,
    },
    /// Faucet account.
    Faucet {
        /// Address to send tokens to.
        address: AccountId32,
    },
}

#[tokio::main]
async fn main() -> Result<(), Error> {
    let cli = Cli::parse();
    if let Commands::Config {} = cli.command {
        eprint!("{}", toml::to_string_pretty(&config::Config::default())?);
        return Ok(());
    }
    let cfg: config::Config = || -> Result<config::Config, Error> {
        let buf = match std::fs::read_to_string(cli.config) {
            Ok(buf) => buf,
            Err(ref e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(config::Config::default())
            }
            Err(e) => return Err(e.into()),
        };
        let cfg: config::Config = toml::from_str(&buf)?;
        Ok(cfg)
    }()?;
    env_logger::builder().filter_level(cfg.log_level.parse::<Level>()?.to_level_filter()).init();
    // Offline commands.
    let mut offline_was_executed = true;
    match cli.command {
        Commands::NewAccount { faucet } => {
            let phrase = bip39::Mnemonic::generate(12)?;
            let keypair = Keypair::from_phrase(&phrase, None)?;
            let account_id: AccountId32 =
                <subxt_signer::sr25519::Keypair as Signer<SUBXTConfig>>::account_id(&keypair);
            eprintln!("Seed phrase: {}", phrase);
            eprintln!("Address: {}", account_id);
            if faucet {
                let cfg = cfg.clone();
                let app: App = App::new(cfg.rpc_url, cfg.signer, cfg.did).await?;
                app.faucet(cfg.faucet, &account_id).await?;
            }
        }
        _ => offline_was_executed = false,
    }
    if offline_was_executed {
        return Ok(());
    }
    let app: App = App::new(cfg.rpc_url, cfg.signer, cfg.did).await?;
    // Online commands.
    match cli.command {
        Commands::Run {} => {
            app.run().await?;
        }
        Commands::Faucet { address } => {
            app.faucet(cfg.faucet, &address).await?;
        }
        _ => (),
    }
    Ok(())
}

struct App {
    api: OnlineClient<SUBXTConfig>,
    rpc: RpcClient,
    rpc_legacy: LegacyRpcMethods<SUBXTConfig>,
    transcoder: ContractMessageTranscoder,
    keypair: Keypair,
    did: config::DID,
}

impl App {
    async fn new(rpc_url: String, signer: config::Signer, did: config::DID) -> Result<Self, Error> {
        let api = OnlineClient::<SUBXTConfig>::from_url(&rpc_url).await?;
        let rpc = rpc::RpcClient::from_url(rpc_url).await?;
        let rpc_legacy: LegacyRpcMethods<SUBXTConfig> = LegacyRpcMethods::new(rpc.clone());
        let transcoder = ContractMessageTranscoder::load(did.metadata_path.clone())?;
        let keypair = match signer.typ {
            config::SignerType::SecretUri => Keypair::from_uri(&SecretUri::from_str(&signer.val)?)?,
            config::SignerType::Phrase => {
                Keypair::from_phrase(&bip39::Mnemonic::from_str(&signer.val)?, None)?
            }
        };
        Ok(Self {
            api,
            rpc,
            rpc_legacy,
            transcoder,
            keypair,
            did,
        })
    }

    async fn faucet(&self, cfg: config::Faucet, address: &AccountId32) -> Result<(), Error> {
        let faucet = Keypair::from_uri(&SecretUri::from_str(&cfg.secret_uri)?)?;
        let faucet_account_id: AccountId32 =
            <subxt_signer::sr25519::Keypair as Signer<SUBXTConfig>>::account_id(&faucet);
        let balance = self.get_balance(&faucet_account_id).await?;
        info!("faucet balance: {}: {}", faucet_account_id, balance.as_dot());

        let balance = self.get_balance(address).await?;
        info!("address balance before: {}", balance.as_dot());
        let tx = crate::did::api::tx().balances().transfer_allow_death(
            MultiAddress::Id(address.clone()),
            Balance::from_dot(cfg.amount as u128).as_planck().raw,
        );
        self.submit_tx(&tx, &faucet).await?;
        let balance = self.get_balance(address).await?;
        info!("address balance after: {}", balance.as_dot());
        Ok(())
    }

    async fn run(&self) -> Result<(), Error> {
        let mut i: usize = 0;
        if self.did.sync {
            self.sync().await?;
        }
        if self.did.explorer {
            loop {
                let res: Result<H256, subxt::Error> =
                    self.rpc.request("chain_getBlockHash", rpc_params![i]).await;
                if let Err(e) = res {
                    match e {
                        subxt::Error::Serialization(_) => return Ok(()),
                        _ => return Err(e.to_string().into()),
                    }
                }
                let hash = res?;
                let block = self.api.blocks().at(hash).await?;
                let events = block.events().await?;
                debug!("found {:?} events in {:?}", events.len(), block.number());
                self.process_events(&events)?;
                i += 1;
            }
        }
        Ok(())
    }

    fn process_events(&self, events: &Events<SUBXTConfig>) -> Result<(), Error> {
        for event in events.iter() {
            let event = event?;
            if let Ok(evt) = event.as_root_event::<RuntimeEvent>() {
                self.process_event(evt)?
            }
        }
        Ok(())
    }

    fn process_event(&self, evt: RuntimeEvent) -> Result<(), Error> {
        match evt {
            RuntimeEvent::Contracts(ContractsEvent::ContractEmitted { contract, data }) => {
                if contract != self.did.contract_address || data.is_empty() {
                    return Ok(());
                }
                let event: DIDEvent = DIDEvent::from(data[0]);
                let mut buf = vec![0; data.len() - 1];
                buf.copy_from_slice(&data[1..]);
                match event {
                    DIDEvent::BeforeFlipping => {
                        let data = BeforeFlipping::decode_all(&mut buf.as_slice())?;
                        debug!("before flipping event: {}", data);
                    }
                    DIDEvent::AfterFlipping => {
                        let data = AfterFlipping::decode_all(&mut buf.as_slice())?;
                        debug!("after flipping event: {}", data);
                    }
                }
            }
            RuntimeEvent::Balances(evt) => match evt {
                BalancesEvent::Withdraw { who, amount } => {
                    debug!("withdraw; from: {}; amount: {}", who, Balance::from_planck(amount))
                }
                BalancesEvent::Transfer { from, to, amount } => {
                    debug!(
                        "transfer; from: {}; to: {}; amount: {}",
                        from,
                        to,
                        Balance::from_planck(amount).as_dot()
                    )
                }
                _ => (),
            },
            _ => trace!("runtime event: {:?}", evt),
        }
        Ok(())
    }

    async fn sync(&self) -> Result<(), Error> {
        let val = self.get().await?;
        debug!("value before executing: {:?}", val);
        self.flip().await?;
        let val = self.get().await?;
        debug!("value after executing: {:?}", val);
        Ok(())
    }

    async fn flip(&self) -> Result<(), Error> {
        let message = "flip";
        let input_data_args: Vec<String> = vec![];
        let dry_run_res = self.dry_run(message, input_data_args.clone()).await?;
        let data = self.transcoder.encode(message, input_data_args)?;
        let call = (TransactionApi {}).contracts().call(
            MultiAddress::Id(self.did.contract_address.clone()),
            0,
            dry_run_res.gas_required.into(),
            None,
            data,
        );
        self.submit_tx(&call, &self.keypair).await
    }

    async fn get(&self) -> Result<bool, Error> {
        const MESSAGE: &str = "get";
        let input_data_args: Vec<String> = vec![];
        let res = self.dry_run(MESSAGE, input_data_args).await?;
        res.to_get_message_res()
    }

    async fn submit_tx<Call: TxPayload, S: Signer<SUBXTConfig>>(
        &self,
        call: &Call,
        signer: &S,
    ) -> Result<(), Error> {
        let account_id = signer.account_id();
        let account_nonce = self.get_nonce(&account_id).await?;
        let mut tx = self
            .api
            .tx()
            .create_signed_with_nonce(call, signer, account_nonce, Default::default())?
            .submit_and_watch()
            .await?;
        while let Some(status) = tx.next().await {
            match status? {
                TxStatus::InBestBlock(tx_in_block) | TxStatus::InFinalizedBlock(tx_in_block) => {
                    let events = tx_in_block.wait_for_success().await?;
                    self.process_events(events.all_events_in_block())?;
                    return Ok(());
                }
                TxStatus::Error { message } => return Err(TransactionError::Error(message).into()),
                TxStatus::Invalid { message } => {
                    return Err(TransactionError::Invalid(message).into())
                }
                TxStatus::Dropped { message } => {
                    return Err(TransactionError::Dropped(message).into())
                }
                _ => continue,
            }
        }
        Err(RpcError::SubscriptionDropped.into())
    }

    async fn get_nonce(&self, account_id: &AccountId32) -> Result<u64, Error> {
        let best_block = self
            .rpc_legacy
            .chain_get_block_hash(None)
            .await?
            .ok_or(subxt::Error::Other("best block not found".into()))?;
        let account_nonce =
            self.api.blocks().at(best_block).await?.account_nonce(account_id).await?;
        Ok(account_nonce)
    }

    async fn get_balance(&self, address: &AccountId32) -> Result<Balance, Error> {
        let best_block = self
            .rpc_legacy
            .chain_get_block_hash(None)
            .await?
            .ok_or(subxt::Error::Other("best block not found".into()))?;
        let balance_address = crate::did::api::storage().system().account(address);
        let info = self.api.storage().at(best_block).fetch(&balance_address).await?;
        if let Some(info) = info {
            return Ok(Balance::from_planck(info.data.free));
        }
        warn!("account is not initialized?: {}", address.to_string());
        Ok(Balance::from_planck(0))
    }

    async fn dry_run(
        &self,
        message: &str,
        input_data_args: Vec<String>,
    ) -> Result<DryRunResult, Error> {
        let input_data = self.transcoder.encode(message, input_data_args)?;
        let args = Call {
            origin: <subxt_signer::sr25519::Keypair as Signer<SUBXTConfig>>::account_id(
                &self.keypair,
            ),
            dest: self.did.contract_address.clone(),
            gas_limit: None,
            storage_deposit_limit: None,
            value: 0,
            input_data,
        }
        .encode();
        let bytes = self.rpc_legacy.state_call("ContractsApi_call", Some(&args), None).await?;
        let exec_res: ContractExecResult<u128, EventRecord<RuntimeEvent, H256>> =
            scale::decode_from_bytes(bytes.clone().into())?;
        let exec_res_data = exec_res.result.unwrap();
        let data = self.transcoder.decode_return(message, &mut exec_res_data.data.as_ref())?;
        debug!("message logs: {}: {:?}", message, from_utf8(&exec_res.debug_message).unwrap());
        Ok(DryRunResult {
            data,
            gas_required: Weight::new(
                exec_res.gas_required.ref_time(),
                exec_res.gas_required.proof_size(),
            ),
        })
    }
}

// All provisioner config related source code is here.
mod config {
    use std::collections::HashMap;

    use log::Level;
    use subxt::utils::AccountId32;

    use crate::Balance;

    #[derive(serde::Serialize, serde::Deserialize, Clone)]
    pub(crate) struct Config {
        pub(crate) log_level: String,
        pub(crate) rpc_url: String,
        pub(crate) signer: Signer,
        pub(crate) faucet: Faucet,
        pub(crate) did: DID,
    }

    impl Default for Config {
        fn default() -> Self {
            Self {
                log_level: Level::Debug.to_string(),
                rpc_url: "ws://127.0.0.1:9944".to_string(),
                signer: Default::default(),
                faucet: Default::default(),
                did: Default::default(),
            }
        }
    }

    #[derive(serde::Serialize, serde::Deserialize, Clone)]
    pub(crate) enum SignerType {
        SecretUri,
        Phrase,
    }

    #[derive(serde::Serialize, serde::Deserialize, Clone)]
    pub(crate) struct Signer {
        pub(crate) typ: SignerType,
        pub(crate) val: String,
    }

    impl Default for Signer {
        fn default() -> Self {
            Self {
                typ: SignerType::SecretUri,
                val: "//Alice".to_string(),
            }
        }
    }

    #[allow(clippy::upper_case_acronyms)]
    #[derive(serde::Serialize, serde::Deserialize, Clone)]
    pub(crate) struct DID {
        pub(crate) sync: bool,
        pub(crate) explorer: bool,
        pub(crate) contract_address: AccountId32,
        pub(crate) metadata_path: String,
        pub(crate) attributes: Attributes,
    }

    impl Default for DID {
        fn default() -> Self {
            Self {
                sync: true,
                explorer: true,
                contract_address: "5H4UGYpLFL2aobsv71CsiFwfcXe9yoSMGtrc6VENGzGRyQZa"
                    .parse()
                    .unwrap(),
                metadata_path: "assets/did.metadata.json".to_string(),
                attributes: Attributes::default(),
            }
        }
    }

    // All fields are required attributes for every DID.
    // Only "additional" is additional.
    #[derive(serde::Serialize, serde::Deserialize, Clone, Default)]
    pub(crate) struct Attributes {
        pub(crate) data_type: String,
        pub(crate) location: String,
        pub(crate) price_access: String,
        pub(crate) pin_access: String,
        pub(crate) additional: Option<HashMap<String, toml::Value>>,
    }

    #[derive(serde::Serialize, serde::Deserialize, Clone)]
    pub(crate) struct Faucet {
        pub(crate) secret_uri: String,
        // We store amount as DOT.
        pub(crate) amount: u64,
    }

    impl Default for Faucet {
        fn default() -> Self {
            Self {
                secret_uri: "//Alice".to_string(),
                amount: Balance::from_dot(100_000).raw as u64,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn balance_conversion() {
        let planck = Balance::from_planck(1000000000000);
        let dot = planck.as_dot();
        assert_eq!(100, dot.raw);

        let dot = Balance::from_dot(100);
        let planck = dot.as_planck();
        assert_eq!(1000000000000, planck.raw);
    }
}
