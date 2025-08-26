use std::{
    cmp::Reverse,
    io::Read,
    str::FromStr,
    sync::{Arc, Mutex},
};

use bdk_chain::{
    bitcoin::{bip32::Xpriv, FeeRate}, Anchor
};

use bdk_wallet::{
    bitcoin::{
        self, consensus::{Decodable, Encodable}, hashes::Hash, network::ParseNetworkError, Address, BlockHash, Network, Transaction
    },
    rusqlite::Connection, template::{Bip86, DescriptorTemplate}, KeychainKind, SignOptions
};
use bincode::Options;
use rand::RngCore;

uniffi::include_scaffolding!("bdkgo");

const DB_MAGIC: &str = "utreexod.bdk.345e94cf";
const DB_MAGIC_LEN: usize = DB_MAGIC.len();
const ENTROPY_LEN: usize = 16; // 12 words

type PBdkWallet = bdk_wallet::PersistedWallet<Connection>;

fn bincode_config() -> impl bincode::Options {
    bincode::options().with_fixint_encoding()
}

#[derive(Debug, thiserror::Error)]
pub enum CreateNewError {
    #[error("failed to parse network type string: {0}")]
    ParseNetwork(ParseNetworkError),
    #[error("failed to parse genesis hash: {0}")]
    ParseGenesisHash(bdk_wallet::bitcoin::hashes::FromSliceError),
    #[error("failed to create new db file: {0}")]
    Database(bdk_chain::rusqlite::Error),
    #[error("failed to init wallet: {0}")]
    Wallet(bdk_wallet::CreateWithPersistError<bdk_chain::rusqlite::Error>),
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("failed to load db: {0}")]
    Database(bdk_chain::rusqlite::Error),
    #[error("failed to read wallet header: {0}")]
    ReadHeader(std::io::Error),
    #[error("failed to decode wallet header: {0}")]
    ParseHeader(bincode::Error),
    #[error("wallet header version unsupported")]
    HeaderVersion,
    #[error("failed to init wallet: {0}")]
    Wallet(bdk_wallet::LoadWithPersistError<bdk_chain::rusqlite::Error>),
}

#[derive(Debug, thiserror::Error)]
pub enum DatabaseError {
    #[error("failed to write to db: {0}")]
    Write(bdk_chain::rusqlite::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyBlockError {
    #[error("failed to decode block: {0}")]
    DecodeBlock(bdk_wallet::bitcoin::consensus::encode::Error),
    #[error("block cannot connect with wallet's chain: {0}")]
    CannotConnect(bdk_chain::local_chain::CannotConnectError),
    #[error("failed to write block to db: {0}")]
    Database(bdk_chain::rusqlite::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum ApplyMempoolError {
    #[error("failed to write mempool txs to db: {0}")]
    Database(bdk_chain::rusqlite::Error),
}

#[derive(Debug, thiserror::Error)]
pub enum CreateTxError {
    #[error("recipient address is invalid: {0}")]
    InvalidAddress(bdk_wallet::bitcoin::address::ParseError),
    #[error("failed to create tx: {0}")]
    CreateTx(bdk_wallet::error::CreateTxError<>),
    #[error("failed to sign tx: {0}")]
    SignTx(bdk_wallet::signer::SignerError),
}
pub struct AddressInfo {
    pub index: u32,
    pub address: String,
}

#[derive(Debug, serde::Serialize, serde::Deserialize)]
pub struct WalletHeader {
    pub version: [u8; DB_MAGIC_LEN],
    pub entropy: [u8; ENTROPY_LEN],
    pub network: Network,
}

impl WalletHeader {
    pub fn new(network: Network) -> Self {
        let mut version = [0_u8; DB_MAGIC_LEN];
        version.copy_from_slice(DB_MAGIC.as_bytes());
        let mut entropy = [0_u8; ENTROPY_LEN];
        rand::thread_rng().fill_bytes(&mut entropy);
        Self {
            version,
            entropy,
            network,
        }
    }

    pub fn encode(&mut self) -> Vec<u8> {
        self.version.copy_from_slice(DB_MAGIC.as_bytes());
        let b = bincode_config()
            .serialize(&self)
            .expect("bincode must serialize");
        let l = (b.len() as u32).to_le_bytes();
        l.into_iter().chain(b).collect::<Vec<u8>>()
    }

    pub fn decode<R: Read>(mut r: R) -> Result<Self, LoadError> {
        let mut l_buf = [0_u8; 4];
        r.read_exact(&mut l_buf)
            .map_err(|err| LoadError::ReadHeader(err))?;
        let l = u32::from_le_bytes(l_buf);
        let mut b = vec![0; l as usize];
        r.read_exact(&mut b)
            .map_err(|err| LoadError::ReadHeader(err))?;

        let header = bincode_config()
            .deserialize::<WalletHeader>(&b)
            .map_err(LoadError::ParseHeader)?;
        if header.version != DB_MAGIC.as_bytes() {
            return Err(LoadError::HeaderVersion);
        }

        Ok(header)
    }

    pub fn descriptor(&self, keychain: KeychainKind) -> String {
        let xpriv: Xpriv = Xpriv::new_master(self.network, &self.entropy).unwrap();
        let (descriptor, key_map, _) = Bip86(xpriv, keychain)
            .build(self.network)
            .expect("Failed to build descriptor");

        descriptor.to_string_with_secret(&key_map)
    }

    pub fn mnemonic_words(&self) -> Vec<String> {
        let mnemonic =
            bdk_wallet::keys::bip39::Mnemonic::from_entropy(&self.entropy).expect("must get mnemonic");
        mnemonic.words().map(|w| w.to_string()).collect()
    }
}

pub struct Wallet {
    inner: Mutex<PBdkWallet>,
    conn: Mutex<Connection>,
    header: Mutex<WalletHeader>,
}

impl Wallet {
    /// Increments the `Arc` pointer exposed via uniffi.
    ///
    /// This is due to a bug with golang uniffi where decrementing this counter is too aggressive.
    /// The caveat of this is that `Wallet` will never be destroyed. This is an okay sacrifice as
    /// typically you want to keep the wallet for the lifetime of the node.
    pub fn increment_reference_counter(self: &Arc<Self>) {
        unsafe { Arc::increment_strong_count(Arc::into_raw(Arc::clone(self))) }
    }

    pub fn create_new(
        db_path: String,
        network: String,
    ) -> Result<Self, CreateNewError> {
        let network = Network::from_str(&network).map_err(CreateNewError::ParseNetwork)?;

        let mut header = WalletHeader::new(network);
        let header_bytes = header.encode();
        let mut conn = match Connection::open(&db_path) {
            Ok(c) => c,
            Err(err) => {
                let _ = std::fs::remove_file(db_path);
                return Err(CreateNewError::Database(err));
            }
        };

         match conn.execute(
            "CREATE TABLE IF NOT EXISTS header (
                data BLOB NOT NULL
            )",
            (),
        ) {
            Ok(w) => w,
            Err(err) => {
                let _ = std::fs::remove_file(db_path);
                return Err(CreateNewError::Database(err));
            }
        };

        match conn.execute(
            "INSERT INTO header (data) VALUES (?1)",
            (header_bytes, )
        ) {
            Ok(w) => w,
            Err(err) => {
                let _ = std::fs::remove_file(db_path);
                return Err(CreateNewError::Database(err));
            }
        };

        let wallet = match bdk_wallet::Wallet::create(
            header.descriptor(KeychainKind::External),
            header.descriptor(KeychainKind::Internal))
            .network(network)
            .create_wallet(&mut conn)
        {
            Ok(w) => w,
            Err(err) => {
                let _ = std::fs::remove_file(db_path);
                return Err(CreateNewError::Wallet(err));
            }
        };

        let inner = Mutex::new(wallet);
        let header = Mutex::new(header);
        let conn = Mutex::new(conn);
        Ok(Self { inner, conn, header })
    }

    pub fn load(db_path: String, genesis_hash: Vec<u8>) -> Result<Self, LoadError> {
        let mut conn = bdk_wallet::rusqlite::Connection::open(&db_path).map_err(LoadError::Database)?;

        let header = {
            let mut stmt = conn.prepare("SELECT data FROM header LIMIT 1").map_err(LoadError::Database)?;
            let result: Vec<u8> = stmt.query_row([], |row| row.get(0)).map_err(LoadError::Database)?;
            let bytes: &[u8] = &result;
            WalletHeader::decode(bytes)?
        };

        let wallet = match bdk_wallet::Wallet::load()
            // check loaded descriptors matches these values and extract private keys
            .descriptor(KeychainKind::External, Some(header.descriptor(KeychainKind::External)))
            .descriptor(KeychainKind::Internal, Some(header.descriptor(KeychainKind::Internal)))
            .extract_keys()
            // ensure loaded wallet's genesis hash matches this value
            .check_genesis_hash(BlockHash::from_slice(&genesis_hash).unwrap())
            // set a lookahead for our indexer
            .lookahead(101)
            .load_wallet(&mut conn)
        {
            Ok(w) => w.unwrap(),
            Err(err) => {
                return Err(LoadError::Wallet(err));
            }
        };

        let inner = Mutex::new(wallet);
        let header = Mutex::new(header);
        let conn = Mutex::new(conn);
        Ok(Self { inner, conn, header })
    }

    pub fn last_unused_address(self: Arc<Self>) -> Result<AddressInfo, DatabaseError> {
        self.increment_reference_counter();
        let mut wallet = self.inner.lock().unwrap();
        let bdk_addr = wallet.next_unused_address(bdk_wallet::KeychainKind::External);
        Ok(AddressInfo { index: bdk_addr.index, address: bdk_addr.address.to_string() })
    }

    pub fn fresh_address(self: Arc<Self>) -> Result<AddressInfo, DatabaseError> {
        self.increment_reference_counter();
        let mut wallet = self.inner.lock().unwrap();
        let bdk_addr = wallet.reveal_next_address(bdk_wallet::KeychainKind::External);
        let mut c = self.conn.lock().unwrap();
        wallet.persist(&mut c).unwrap();
        Ok(AddressInfo { index: bdk_addr.index, address: bdk_addr.address.to_string() })
    }

    pub fn peek_address(self: Arc<Self>, index: u32) -> Result<AddressInfo, DatabaseError> {
        self.increment_reference_counter();
        let wallet = self.inner.lock().unwrap();
        let bdk_addr = wallet.peek_address(bdk_wallet::KeychainKind::External, index);
        Ok(AddressInfo { index: bdk_addr.index, address: bdk_addr.address.to_string() })
    }

    pub fn balance(self: Arc<Self>) -> Balance {
        self.increment_reference_counter();
        let wallet = self.inner.lock().unwrap();
        let bdk_balance = wallet.balance();
        Balance {
            immature: bdk_balance.immature.to_sat(),
            trusted_pending: bdk_balance.trusted_pending.to_sat(),
            untrusted_pending: bdk_balance.untrusted_pending.to_sat(),
            confirmed: bdk_balance.confirmed.to_sat(),
        }
    }

    pub fn genesis_hash(self: Arc<Self>) -> Vec<u8> {
        self.increment_reference_counter();
        self.inner
            .lock()
            .unwrap()
            .local_chain()
            .genesis_hash()
            .to_byte_array()
            .to_vec()
    }

    pub fn recent_blocks(self: Arc<Self>, count: u32) -> Vec<BlockId> {
        self.increment_reference_counter();
        let tip = self.inner.lock().unwrap().latest_checkpoint();
        tip.into_iter()
            .take(count as _)
            .map(|cp| BlockId {
                height: cp.height(),
                hash: cp.hash().to_byte_array().to_vec(),
            })
            .collect()
    }

    pub fn apply_block(
        self: Arc<Self>,
        height: u32,
        block_bytes: &[u8],
    ) -> Result<ApplyResult, ApplyBlockError> {
        self.increment_reference_counter();

        let mut wallet = self.inner.lock().unwrap();

        let block = bitcoin::Block::consensus_decode_from_finite_reader(&mut block_bytes.as_ref())
            .map_err(ApplyBlockError::DecodeBlock)?;

        let tip = wallet.latest_checkpoint();

        if tip.height() == 0 {
            wallet
                .apply_block_connected_to(&block, height, tip.block_id())
                .map_err(|err| match err {
                    bdk_wallet::chain::local_chain::ApplyHeaderError::InconsistentBlocks => {
                        unreachable!("cannot happen")
                    }
                    bdk_wallet::chain::local_chain::ApplyHeaderError::CannotConnect(err) => {
                        ApplyBlockError::CannotConnect(err)
                    }
                })?;
        } else {
            wallet
                .apply_block(&block, height)
                .map_err(ApplyBlockError::CannotConnect)?;
        }
        let mut c = self.conn.lock().unwrap();
        wallet.persist(&mut c).map_err(ApplyBlockError::Database)?;

        let res = ApplyResult::new(&wallet);
        Ok(res)
    }


    pub fn apply_mempool(
        self: Arc<Self>,
        txs: Vec<MempoolTx>,
    ) -> Result<ApplyResult, ApplyMempoolError> {
        self.increment_reference_counter();
        let mut wallet = self.inner.lock().unwrap();
        let txs = txs
            .into_iter()
            .map(|mtx| {
                (
                    Transaction::consensus_decode_from_finite_reader(&mut mtx.tx.as_slice())
                        .expect("must decode tx"),
                    mtx.added_unix,
                )
            })
            .collect::<Vec<_>>();
        wallet.apply_unconfirmed_txs(txs.iter().map(|(tx, added)| (tx.clone(), added.clone())));

        let mut c = self.conn.lock().unwrap();
        wallet.persist(&mut c).map_err(ApplyMempoolError::Database)?;

        let res = ApplyResult::new(&wallet);
        Ok(res)
    }

    pub fn create_tx(
        self: Arc<Self>,
        feerate: u64,
        recipients: Vec<Recipient>,
    ) -> Result<Vec<u8>, CreateTxError> {
        self.increment_reference_counter();
        let mut wallet = self.inner.lock().unwrap();

        let recipients = recipients
            .into_iter()
            .map(|r| -> Result<_, _> {
                let addr = Address::from_str(&r.address)
                    .map_err(CreateTxError::InvalidAddress)?
                    .require_network(wallet.network())
                    .map_err(CreateTxError::InvalidAddress)?;
                Ok((addr.script_pubkey().into(), bdk_chain::bitcoin::Amount::from_sat(r.amount)))
            })
            .collect::<Result<Vec<_>, _>>()?;

        let mut txb = wallet.build_tx();
        txb.set_recipients(recipients);
        txb.fee_rate(FeeRate::from_sat_per_vb(feerate).unwrap());
        let mut psbt = txb.finish().map_err(CreateTxError::CreateTx)?;

        let is_finalized = wallet
            .sign(&mut psbt, SignOptions::default())
            .map_err(CreateTxError::SignTx)?;
        assert!(is_finalized, "tx should always be finalized");

        let mut raw_bytes = Vec::<u8>::new();
        psbt.extract_tx()
            .unwrap()
            .consensus_encode(&mut raw_bytes)
            .expect("must encode tx");
        Ok(raw_bytes)
    }

    pub fn mnemonic_words(self: Arc<Self>) -> Vec<String> {
        self.increment_reference_counter();
        self.header.lock().unwrap().mnemonic_words()
    }

    pub fn transactions(self: Arc<Self>) -> Vec<TxInfo> {
        self.increment_reference_counter();
        let wallet = self.inner.lock().unwrap();
        let height = wallet.latest_checkpoint().height();
        let mut txs = wallet
            .transactions()
            .map(|ctx| {
                let txid = ctx.tx_node.txid.to_byte_array().to_vec();
                let mut tx = Vec::<u8>::new();
                ctx.tx_node
                    .tx
                    .consensus_encode(&mut tx)
                    .expect("must encode");
                let (spent, received) = wallet.sent_and_received(&ctx.tx_node.tx);
                let confirmations = ctx
                    .chain_position
                    .confirmation_height_upper_bound()
                    .map_or(0, |conf_height| (1 + height).saturating_sub(conf_height));
                TxInfo {
                    txid: txid,
                    tx: tx,
                    spent: spent.to_sat(),
                    received: received.to_sat(),
                    confirmations: confirmations,
                }
            })
            .collect::<Vec<_>>();
        txs.sort_unstable_by_key(|tx| Reverse(tx.confirmations));
        txs
    }

    pub fn utxos(self: Arc<Self>) -> Vec<UtxoInfo> {
        self.increment_reference_counter();
        let wallet = self.inner.lock().unwrap();
        let wallet_height = wallet.latest_checkpoint().height();
        let mut utxos = wallet
            .list_unspent()
            .map(|utxo| UtxoInfo {
                txid: utxo.outpoint.txid.to_byte_array().to_vec(),
                vout: utxo.outpoint.vout,
                amount: utxo.txout.value.to_sat(),
                script_pubkey: utxo.txout.script_pubkey.to_bytes(),
                is_change: utxo.keychain == KeychainKind::Internal,
                derivation_index: utxo.derivation_index,
                confirmations: match utxo.chain_position {
                    bdk_chain::ChainPosition::Confirmed { anchor, .. } => {
                        (1 + wallet_height).saturating_sub(anchor.confirmation_height_upper_bound())
                    }
                    bdk_chain::ChainPosition::Unconfirmed { .. } => 0,
                },
            })
            .collect::<Vec<_>>();
        utxos.sort_unstable_by_key(|utxo| Reverse(utxo.confirmations));
        utxos
    }
}

pub struct Balance {
    pub immature: u64,
    pub trusted_pending: u64,
    pub untrusted_pending: u64,
    pub confirmed: u64,
}

pub struct Recipient {
    pub address: String,
    pub amount: u64,
}

pub struct BlockId {
    pub height: u32,
    pub hash: Vec<u8>,
}

pub struct TxInfo {
    pub txid: Vec<u8>,
    pub tx: Vec<u8>,
    /// Sum of inputs spending from owned script pubkeys.
    pub spent: u64,
    /// Sum of outputs containing owned script pubkeys.
    pub received: u64,
    /// How confirmed is this transaction?
    pub confirmations: u32,
}

pub struct UtxoInfo {
    pub txid: Vec<u8>,
    pub vout: u32,
    pub amount: u64,
    pub script_pubkey: Vec<u8>,
    pub is_change: bool,
    pub derivation_index: u32,
    pub confirmations: u32,
}

pub struct MempoolTx {
    pub tx: Vec<u8>,
    pub added_unix: u64,
}

pub struct ApplyResult {
    pub relevant_txids: Vec<Vec<u8>>,
}

impl ApplyResult {
    pub fn new(wallet: &PBdkWallet) -> Self {
        let relevant_txids = wallet
            .staged()
            .map(|staged| {
                staged.tx_graph
                    .txs
                    .iter()
                    .map(|tx| tx.compute_txid().to_byte_array().to_vec())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();

        Self { relevant_txids }
    }
}
