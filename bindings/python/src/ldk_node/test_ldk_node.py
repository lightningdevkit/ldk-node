import unittest
import tempfile
import time
import subprocess
import os
import re
import requests
import asyncio
import threading

import ldk_node
from ldk_node import *
from kv_store import TestKvStore

DEFAULT_ESPLORA_SERVER_URL = "http://127.0.0.1:3002"
DEFAULT_TEST_NETWORK = Network.REGTEST
DEFAULT_BITCOIN_CLI_BIN = "bitcoin-cli"

class NodeSetup:
    def __init__(self, node, node_id, tmp_dir, listening_addresses, stores=None):
        self.node = node
        self.node_id = node_id
        self.tmp_dir = tmp_dir
        self.listening_addresses = listening_addresses
        self.stores = stores  # (primary, backup, ephemeral) or None
        
    def cleanup(self):
        self.node.stop()
        time.sleep(1)
        self.tmp_dir.cleanup()

def bitcoin_cli(cmd):
    args = []

    bitcoin_cli_bin = [DEFAULT_BITCOIN_CLI_BIN]
    if os.environ.get('BITCOIN_CLI_BIN'):
        bitcoin_cli_bin = os.environ['BITCOIN_CLI_BIN'].split()

    args += bitcoin_cli_bin
    args += ["-regtest"]

    if os.environ.get('BITCOIND_RPC_USER'):
        args += ["-rpcuser=" + os.environ['BITCOIND_RPC_USER']]

    if os.environ.get('BITCOIND_RPC_PASSWORD'):
        args += ["-rpcpassword=" + os.environ['BITCOIND_RPC_PASSWORD']]

    for c in cmd.split():
        args += [c]

    print("RUNNING:", args)
    res = subprocess.run(args, capture_output=True)
    return str(res.stdout.decode("utf-8"))

def mine(blocks):
    address = bitcoin_cli("getnewaddress").strip()
    mining_res = bitcoin_cli("generatetoaddress " + str(blocks) + " " + str(address))
    print("MINING_RES:", mining_res)

    m = re.search("\\n.+\n\\]$", mining_res)
    last_block = str(m.group(0))
    return str(last_block.strip().replace('"','').replace('\n]',''))

def mine_and_wait(esplora_endpoint, blocks):
    last_block = mine(blocks)
    wait_for_block(esplora_endpoint, last_block)

def wait_for_block(esplora_endpoint, block_hash):
    url = esplora_endpoint + "/block/" + block_hash + "/status"
    attempts = 0
    max_attempts = 30

    while attempts < max_attempts:
        try:
            res = requests.get(url, timeout=10)
            json = res.json()
            if json.get('in_best_chain'):
                return

        except Exception as e:
            print(f"Error: {e}")

        attempts += 1
        time.sleep(0.5)

    raise Exception(f"Failed to confirm block {block_hash} after {max_attempts} attempts")

def wait_for_tx(esplora_endpoint, txid):
    url = esplora_endpoint + "/tx/" + txid
    attempts = 0
    max_attempts = 30

    while attempts < max_attempts:
        try:
            res = requests.get(url, timeout=10)
            json = res.json()
            if json.get('txid') == txid:
                return

        except Exception as e:
            print(f"Error: {e}")

        attempts += 1
        time.sleep(0.5)

    raise Exception(f"Failed to confirm transaction {txid} after {max_attempts} attempts")

def send_to_address(address, amount_sats):
    amount_btc = amount_sats/100000000.0
    cmd = "sendtoaddress " + str(address) + " " + str(amount_btc)
    res = str(bitcoin_cli(cmd)).strip()
    print("SEND TX:", res)
    return res

def setup_node(tmp_dir, esplora_endpoint, listening_addresses):
    mnemonic = generate_entropy_mnemonic(None)
    node_entropy = NodeEntropy.from_bip39_mnemonic(mnemonic, None)
    config = default_config()
    builder = Builder.from_config(config)
    builder.set_storage_dir_path(tmp_dir)
    builder.set_chain_source_esplora(esplora_endpoint, None)
    builder.set_network(DEFAULT_TEST_NETWORK)
    builder.set_listening_addresses(listening_addresses)
    return builder.build(node_entropy)

def setup_two_nodes(esplora_endpoint, port_1=2323, port_2=2324, use_tier_store=False) -> tuple[NodeSetup, NodeSetup]:
    # Setup Node 1
    tmp_dir_1 = tempfile.TemporaryDirectory("_ldk_node_1")
    print("TMP DIR 1:", tmp_dir_1.name)
    
    listening_addresses_1 = [f"127.0.0.1:{port_1}"]
    if use_tier_store:
        node_1, stores_1 = setup_node_with_tier_store(tmp_dir_1.name, esplora_endpoint, listening_addresses_1)
    else:
        node_1 = setup_node(tmp_dir_1.name, esplora_endpoint, listening_addresses_1)
        stores_1 = None
    
    node_1.start()
    node_id_1 = node_1.node_id()
    print("Node ID 1:", node_id_1)
    
    setup_1 = NodeSetup(node_1, node_id_1, tmp_dir_1, listening_addresses_1, stores_1)
    
    # Setup Node 2
    tmp_dir_2 = tempfile.TemporaryDirectory("_ldk_node_2")
    print("TMP DIR 2:", tmp_dir_2.name)
    
    listening_addresses_2 = [f"127.0.0.1:{port_2}"]
    if use_tier_store:
        node_2, stores_2 = setup_node_with_tier_store(tmp_dir_2.name, esplora_endpoint, listening_addresses_2)
    else:
        node_2 = setup_node(tmp_dir_2.name, esplora_endpoint, listening_addresses_2)
        stores_2 = None
    
    node_2.start()
    node_id_2 = node_2.node_id()
    print("Node ID 2:", node_id_2)
    
    setup_2 = NodeSetup(node_2, node_id_2, tmp_dir_2, listening_addresses_2, stores_2)
    
    return setup_1, setup_2

def setup_node_with_tier_store(tmp_dir, esplora_endpoint, listening_addresses) -> tuple[Node, tuple[TestKvStore, TestKvStore, TestKvStore]]:
    mnemonic = generate_entropy_mnemonic(None)
    node_entropy = NodeEntropy.from_bip39_mnemonic(mnemonic, None)
    config = default_config()

    primary = TestKvStore("primary")
    backup = TestKvStore("backup")
    ephemeral = TestKvStore("ephemeral")

    builder = Builder.from_config(config)
    builder.set_storage_dir_path(tmp_dir)
    builder.set_chain_source_esplora(esplora_endpoint, None)
    builder.set_network(DEFAULT_TEST_NETWORK)
    builder.set_listening_addresses(listening_addresses)
    builder.set_backup_store(FfiDynStore.from_store(backup))
    builder.set_ephemeral_store(FfiDynStore.from_store(ephemeral))
    
    return builder.build_with_store(node_entropy, FfiDynStore.from_store(primary)), (primary, backup, ephemeral)

def do_channel_full_cycle(setup_1: NodeSetup, setup_2: NodeSetup, esplora_endpoint):
    # Fund both nodes
    (node_1, node_2) = (setup_1.node, setup_2.node)
    address_1 = node_1.onchain_payment().new_address()
    txid_1 = send_to_address(address_1, 100000)
    address_2 = node_2.onchain_payment().new_address()
    txid_2 = send_to_address(address_2, 100000)

    wait_for_tx(esplora_endpoint, txid_1)
    wait_for_tx(esplora_endpoint, txid_2)

    mine_and_wait(esplora_endpoint, 6)

    node_1.sync_wallets()
    node_2.sync_wallets()

    spendable_balance_1 = node_1.list_balances().spendable_onchain_balance_sats
    spendable_balance_2 = node_2.list_balances().spendable_onchain_balance_sats
    total_balance_1 = node_1.list_balances().total_onchain_balance_sats
    total_balance_2 = node_2.list_balances().total_onchain_balance_sats

    print("SPENDABLE 1:", spendable_balance_1)
    assert spendable_balance_1 == 100000

    print("SPENDABLE 2:", spendable_balance_2)
    assert spendable_balance_2 == 100000

    print("TOTAL 1:", total_balance_1)
    assert total_balance_1 == 100000

    print("TOTAL 2:", total_balance_2)
    assert total_balance_2 == 100000

    (node_id_2, listening_addresses_2) = (setup_2.node_id, setup_2.listening_addresses)
    node_1.open_channel(node_id_2, listening_addresses_2[0], 50000, None, None)

    channel_pending_event_1 = expect_event(node_1, Event.CHANNEL_PENDING)
    channel_pending_event_2 = expect_event(node_2, Event.CHANNEL_PENDING)
    funding_txid = channel_pending_event_1.funding_txo.txid
    wait_for_tx(esplora_endpoint, funding_txid)
    mine_and_wait(esplora_endpoint, 6)

    node_1.sync_wallets()
    node_2.sync_wallets()

    channel_ready_event_1 = expect_event(node_1, Event.CHANNEL_READY)
    print("funding_txo:", funding_txid)

    channel_ready_event_2 = expect_event(node_2, Event.CHANNEL_READY)

    description = Bolt11InvoiceDescription.DIRECT("asdf")
    invoice = node_2.bolt11_payment().receive(2500000, description, 9217)
    node_1.bolt11_payment().send(invoice, None)

    expect_event(node_1, Event.PAYMENT_SUCCESSFUL)

    expect_event(node_2, Event.PAYMENT_RECEIVED)

    node_id_1 = setup_1.node_id
    node_2.close_channel(channel_ready_event_2.user_channel_id, node_id_1)

    # expect channel closed event on both nodes
    expect_event(node_1, Event.CHANNEL_CLOSED)

    expect_event(node_2, Event.CHANNEL_CLOSED)

    mine_and_wait(esplora_endpoint, 1)

    node_1.sync_wallets()
    node_2.sync_wallets()

    spendable_balance_after_close_1 = node_1.list_balances().spendable_onchain_balance_sats
    assert spendable_balance_after_close_1 > 95000
    assert spendable_balance_after_close_1 < 100000
    spendable_balance_after_close_2 = node_2.list_balances().spendable_onchain_balance_sats
    assert spendable_balance_after_close_2 == 102500

def get_esplora_endpoint():
    if os.environ.get('ESPLORA_ENDPOINT'):
        return str(os.environ['ESPLORA_ENDPOINT'])
    return DEFAULT_ESPLORA_SERVER_URL


def expect_event(node, expected_event_type):
    event = node.wait_next_event()
    assert isinstance(event, expected_event_type)
    print("EVENT:", event)
    node.event_handled()
    return event 

class TestLdkNode(unittest.TestCase):
    def setUp(self):
        bitcoin_cli("createwallet ldk_node_test")
        mine(101)
        time.sleep(3)
        esplora_endpoint = get_esplora_endpoint()
        mine_and_wait(esplora_endpoint, 1)

    def test_channel_full_cycle(self):
        esplora_endpoint = get_esplora_endpoint()
        setup_1, setup_2 = setup_two_nodes(esplora_endpoint)
        
        do_channel_full_cycle(setup_1, setup_2, esplora_endpoint)
        
        setup_1.cleanup()
        setup_2.cleanup()

    def test_tier_store(self):
        # Set event loop for async Python callbacks from Rust
        # (https://mozilla.github.io/uniffi-rs/0.27/futures.html#python-uniffi_set_event_loop)
        loop = asyncio.new_event_loop()
        
        def run_loop():
            asyncio.set_event_loop(loop)
            loop.run_forever()
        
        loop_thread = threading.Thread(target=run_loop, daemon=True)
        loop_thread.start()
        ldk_node.uniffi_set_event_loop(loop)
        
        esplora_endpoint = get_esplora_endpoint()
        setup_1, setup_2 = setup_two_nodes(esplora_endpoint, port_1=2325, port_2=2326, use_tier_store=True)
        
        do_channel_full_cycle(setup_1, setup_2, esplora_endpoint)
        
        primary, backup, ephemeral = setup_1.stores
        
        # Wait for async backup
        time.sleep(2)  
        
        self.assertGreater(len(primary.storage), 0, "Primary should have data")
        self.assertGreater(len(backup.storage), 0, "Backup should have data")
        self.assertEqual(list(primary.storage.keys()), list(backup.storage.keys()), 
                        "Backup should mirror primary")
        
        self.assertGreater(len(ephemeral.storage), 0, "Ephemeral should have data")
        ephemeral_keys = [key for namespace in ephemeral.storage.values() for key in namespace.keys()]
        has_scorer_or_graph = any(key in ['scorer', 'network_graph'] for key in ephemeral_keys)
        self.assertTrue(has_scorer_or_graph, "Ephemeral should contain scorer or network_graph data")
        
        setup_1.cleanup()
        setup_2.cleanup()

if __name__ == '__main__':
    unittest.main()
