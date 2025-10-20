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

def setup_two_nodes(esplora_endpoint, port_1=2323, port_2=2324, use_tier_store=False):
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

def setup_node_with_tier_store(tmp_dir, esplora_endpoint, listening_addresses):
    mnemonic = generate_entropy_mnemonic(None)
    node_entropy = NodeEntropy.from_bip39_mnemonic(mnemonic, None)
    config = default_config()

    primary = TestKvStore("primary")
    backup = TestKvStore("backup")
    ephemeral = TestKvStore("ephemeral")
    retry_config = RetryConfig(
        initial_retry_delay_ms=10, 
        maximum_delay_ms=100, 
        backoff_multiplier=2.0
    )

    # Set event loop for async Python callbacks from Rust
    # (https://mozilla.github.io/uniffi-rs/0.27/futures.html#python-uniffi_set_event_loop)
    loop = asyncio.new_event_loop()
    
    def run_loop():
        asyncio.set_event_loop(loop)
        loop.run_forever()
    
    loop_thread = threading.Thread(target=run_loop, daemon=True)
    loop_thread.start()
    ldk_node.uniffi_set_event_loop(loop)

    builder = Builder.from_config(config)
    builder.set_storage_dir_path(tmp_dir)
    builder.set_chain_source_esplora(esplora_endpoint, None)
    builder.set_network(DEFAULT_TEST_NETWORK)
    builder.set_listening_addresses(listening_addresses)
    builder.set_tier_store_retry_config(retry_config)
    builder.set_tier_store_backup(FfiDynStore.from_store(backup))
    builder.set_tier_store_ephemeral(FfiDynStore.from_store(ephemeral))
    
    return builder.build_with_tier_store(node_entropy, FfiDynStore.from_store(primary)), (primary, backup, ephemeral)

def do_channel_full_cycle(setup_1, setup_2, esplora_endpoint):
    # Fund both nodes
    address_1 = setup_1.node.onchain_payment().new_address()
    txid_1 = send_to_address(address_1, 100000)
    address_2 = setup_2.node.onchain_payment().new_address()
    txid_2 = send_to_address(address_2, 100000)

    wait_for_tx(esplora_endpoint, txid_1)
    wait_for_tx(esplora_endpoint, txid_2)
    mine_and_wait(esplora_endpoint, 6)

    setup_1.node.sync_wallets()
    setup_2.node.sync_wallets()

    # Verify balances
    spendable_balance_1 = setup_1.node.list_balances().spendable_onchain_balance_sats
    spendable_balance_2 = setup_2.node.list_balances().spendable_onchain_balance_sats
    assert spendable_balance_1 == 100000
    assert spendable_balance_2 == 100000

    # Open channel
    setup_1.node.open_channel(setup_2.node_id, setup_2.listening_addresses[0], 50000, None, None)

    channel_pending_event_1 = setup_1.node.wait_next_event()
    assert isinstance(channel_pending_event_1, Event.CHANNEL_PENDING)
    setup_1.node.event_handled()

    channel_pending_event_2 = setup_2.node.wait_next_event()
    assert isinstance(channel_pending_event_2, Event.CHANNEL_PENDING)
    setup_2.node.event_handled()

    funding_txid = channel_pending_event_1.funding_txo.txid
    wait_for_tx(esplora_endpoint, funding_txid)
    mine_and_wait(esplora_endpoint, 6)

    setup_1.node.sync_wallets()
    setup_2.node.sync_wallets()

    channel_ready_event_1 = setup_1.node.wait_next_event()
    assert isinstance(channel_ready_event_1, Event.CHANNEL_READY)
    setup_1.node.event_handled()

    channel_ready_event_2 = setup_2.node.wait_next_event()
    assert isinstance(channel_ready_event_2, Event.CHANNEL_READY)
    setup_2.node.event_handled()

    # Make payment
    description = Bolt11InvoiceDescription.DIRECT("asdf")
    invoice = setup_2.node.bolt11_payment().receive(2500000, description, 9217)
    setup_1.node.bolt11_payment().send(invoice, None)

    payment_successful_event_1 = setup_1.node.wait_next_event()
    assert isinstance(payment_successful_event_1, Event.PAYMENT_SUCCESSFUL)
    setup_1.node.event_handled()

    payment_received_event_2 = setup_2.node.wait_next_event()
    assert isinstance(payment_received_event_2, Event.PAYMENT_RECEIVED)
    setup_2.node.event_handled()

    # Close channel
    setup_2.node.close_channel(channel_ready_event_2.user_channel_id, setup_1.node_id)

    channel_closed_event_1 = setup_1.node.wait_next_event()
    assert isinstance(channel_closed_event_1, Event.CHANNEL_CLOSED)
    setup_1.node.event_handled()

    channel_closed_event_2 = setup_2.node.wait_next_event()
    assert isinstance(channel_closed_event_2, Event.CHANNEL_CLOSED)
    setup_2.node.event_handled()

    mine_and_wait(esplora_endpoint, 1)
    setup_1.node.sync_wallets()
    setup_2.node.sync_wallets()

    # Verify final balances
    spendable_balance_after_close_1 = setup_1.node.list_balances().spendable_onchain_balance_sats
    assert spendable_balance_after_close_1 > 95000
    assert spendable_balance_after_close_1 < 100000
    spendable_balance_after_close_2 = setup_2.node.list_balances().spendable_onchain_balance_sats
    assert spendable_balance_after_close_2 == 102500

def get_esplora_endpoint():
    if os.environ.get('ESPLORA_ENDPOINT'):
        return str(os.environ['ESPLORA_ENDPOINT'])
    return DEFAULT_ESPLORA_SERVER_URL

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

