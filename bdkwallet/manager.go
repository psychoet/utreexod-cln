package bdkwallet

import (
	"os"
	"path/filepath"

	"github.com/utreexo/utreexod/blockchain"
	"github.com/utreexo/utreexod/btcutil"
	"github.com/utreexo/utreexod/chaincfg"
	"github.com/utreexo/utreexod/mempool"
)

var defaultWalletPath = "bdkwallet"
var defaultWalletFileName = "default.dat"

// ManagerConfig is a configuration struct used to
type ManagerConfig struct {
	Chain       *blockchain.BlockChain
	TxMemPool   *mempool.TxPool
	ChainParams *chaincfg.Params
	DataDir     string
}

// Manager handles the configuration and handling data in between the utreexo node
// and the bdk wallet library.
type Manager struct {
	config ManagerConfig

	// Wallet is the underlying wallet that calls out to the
	// bdk rust library.
	Wallet Wallet // wallet does not need a mutex as it's done in Rust
}

func WalletDir(dataDir string) string {
	return filepath.Join(dataDir, defaultWalletPath)
}

func DoesWalletDirExist(dataDir string) (bool, error) {
	walletDir := WalletDir(dataDir)
	if _, err := os.Stat(walletDir); err != nil {
		if os.IsNotExist(err) {
			return false, nil
		}
		return false, err
	}
	return true, nil
}

func NewManager(config ManagerConfig) (*Manager, error) {
	factory, err := factory()
	if err != nil {
		return nil, err
	}

	walletDir := WalletDir(config.DataDir)
	if err := os.MkdirAll(walletDir, os.ModePerm); err != nil {
		return nil, err
	}

	dbPath := filepath.Join(walletDir, defaultWalletFileName)
	var wallet Wallet
	if _, err := os.Stat(dbPath); err != nil {
		if !os.IsNotExist(err) {
			return nil, err
		}
		if wallet, err = factory.Create(dbPath, config.ChainParams); err != nil {
			return nil, err
		}
	} else {
		if wallet, err = factory.Load(dbPath, config.ChainParams); err != nil {
			return nil, err
		}
	}

	m := Manager{
		config: config,
		Wallet: wallet,
	}
	if config.Chain != nil {
		// Subscribe to new blocks/reorged blocks.
		config.Chain.Subscribe(m.handleBlockchainNotification)
	}

	log.Info("Started the BDK wallet manager.")
	return &m, nil
}

func (m *Manager) NotifyNewTransactions(txns []*mempool.TxDesc) {
	if m.Wallet == nil {
		return
	}

	if err := m.Wallet.ApplyMempoolTransactions(txns); err != nil {
		log.Errorf("Failed to apply mempool txs to the wallet. %v", err)
	}
}

func (m *Manager) handleBlockchainNotification(notification *blockchain.Notification) {
	if m.Wallet == nil {
		return
	}

	switch notification.Type {
	// A block has been accepted into the block chain.
	case blockchain.NTBlockConnected:
		block, ok := notification.Data.(*btcutil.Block)
		if !ok {
			log.Warnf("Chain connected notification is not a block.")
			return
		}
		err := m.Wallet.ApplyBlock(block)
		if err != nil {
			log.Criticalf("Couldn't apply block to the wallet. %v", err)
		}
	}
}
