import unittest
import tempfile
import time
import subprocess
import os
import re
import requests

from ldk_node import *

DEFAULT_ESPLORA_SERVER_URL = "http://127.0.0.1:3002"
DEFAULT_TEST_NETWORK = Network.REGTEST
DEFAULT_BITCOIN_CLI_BIN = "bitcoin-cli"

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
    esplora_picked_up_block = False
    while not esplora_picked_up_block:
        res = requests.get(url)
        try:
            json = res.json()
            esplora_picked_up_block = json['in_best_chain']
        except:
            pass
        time.sleep(1)

def wait_for_tx(esplora_endpoint, txid):
    url = esplora_endpoint + "/tx/" + txid
    esplora_picked_up_tx = False
    while not esplora_picked_up_tx:
        res = requests.get(url)
        try:
            json = res.json()
            esplora_picked_up_tx = json['txid'] == txid
        except:
            pass
        time.sleep(1)

def send_to_address(address, amount_sats):
    amount_btc = amount_sats/100000000.0
    cmd = "sendtoaddress " + str(address) + " " + str(amount_btc)
    res = str(bitcoin_cli(cmd)).strip()
    print("SEND TX:", res)
    return res


def setup_node(tmp_dir, esplora_endpoint, listening_addresses):
    config = default_config()
    builder = Builder.from_config(config)
    builder.set_storage_dir_path(tmp_dir)
    builder.set_esplora_server(esplora_endpoint)
    builder.set_network(DEFAULT_TEST_NETWORK)
    builder.set_listening_addresses(listening_addresses)
    return builder.build()

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

        ## Setup Node 1
        tmp_dir_1 = tempfile.TemporaryDirectory("_ldk_node_1")
        print("TMP DIR 1:", tmp_dir_1.name)

        listening_addresses_1 = ["127.0.0.1:2323"]
        node_1 = setup_node(tmp_dir_1.name, esplora_endpoint, listening_addresses_1)
        node_1.start()
        node_id_1 = node_1.node_id()
        print("Node ID 1:", node_id_1)

        # Setup Node 2
        tmp_dir_2 = tempfile.TemporaryDirectory("_ldk_node_2")
        print("TMP DIR 2:", tmp_dir_2.name)

        listening_addresses_2 = ["127.0.0.1:2324"]
        node_2 = setup_node(tmp_dir_2.name, esplora_endpoint, listening_addresses_2)
        node_2.start()
        node_id_2 = node_2.node_id()
        print("Node ID 2:", node_id_2)

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
        self.assertEqual(spendable_balance_1, 100000)

        print("SPENDABLE 2:", spendable_balance_2)
        self.assertEqual(spendable_balance_2, 100000)

        print("TOTAL 1:", total_balance_1)
        self.assertEqual(total_balance_1, 100000)

        print("TOTAL 2:", total_balance_2)
        self.assertEqual(total_balance_2, 100000)

        node_1.connect_open_channel(node_id_2, listening_addresses_2[0], 50000, None, None, True)

        channel_pending_event_1 = node_1.wait_next_event()
        assert isinstance(channel_pending_event_1, Event.CHANNEL_PENDING)
        print("EVENT:", channel_pending_event_1)
        node_1.event_handled()

        channel_pending_event_2 = node_2.wait_next_event()
        assert isinstance(channel_pending_event_2, Event.CHANNEL_PENDING)
        print("EVENT:", channel_pending_event_2)
        node_2.event_handled()

        funding_txid = channel_pending_event_1.funding_txo.txid
        wait_for_tx(esplora_endpoint, funding_txid)
        mine_and_wait(esplora_endpoint, 6)

        node_1.sync_wallets()
        node_2.sync_wallets()

        channel_ready_event_1 = node_1.wait_next_event()
        assert isinstance(channel_ready_event_1, Event.CHANNEL_READY)
        print("EVENT:", channel_ready_event_1)
        print("funding_txo:", funding_txid)
        node_1.event_handled()

        channel_ready_event_2 = node_2.wait_next_event()
        assert isinstance(channel_ready_event_2, Event.CHANNEL_READY)
        print("EVENT:", channel_ready_event_2)
        node_2.event_handled()

        invoice = node_2.bolt11_payment().receive(2500000, "asdf", 9217)
        node_1.bolt11_payment().send(invoice)

        payment_successful_event_1 = node_1.wait_next_event()
        assert isinstance(payment_successful_event_1, Event.PAYMENT_SUCCESSFUL)
        print("EVENT:", payment_successful_event_1)
        node_1.event_handled()

        payment_received_event_2 = node_2.wait_next_event()
        assert isinstance(payment_received_event_2, Event.PAYMENT_RECEIVED)
        print("EVENT:", payment_received_event_2)
        node_2.event_handled()

        node_2.close_channel(channel_ready_event_2.user_channel_id, node_id_1, false)

        channel_closed_event_1 = node_1.wait_next_event()
        assert isinstance(channel_closed_event_1, Event.CHANNEL_CLOSED)
        print("EVENT:", channel_closed_event_1)
        node_1.event_handled()

        channel_closed_event_2 = node_2.wait_next_event()
        assert isinstance(channel_closed_event_2, Event.CHANNEL_CLOSED)
        print("EVENT:", channel_closed_event_2)
        node_2.event_handled()

        mine_and_wait(esplora_endpoint, 1)

        node_1.sync_wallets()
        node_2.sync_wallets()

        spendable_balance_after_close_1 = node_1.list_balances().spendable_onchain_balance_sats
        assert spendable_balance_after_close_1 > 95000
        assert spendable_balance_after_close_1 < 100000
        spendable_balance_after_close_2 = node_2.list_balances().spendable_onchain_balance_sats
        self.assertEqual(spendable_balance_after_close_2, 102500)

        # Stop nodes
        node_1.stop()
        node_2.stop()

        # Cleanup
        time.sleep(1) # Wait a sec so our logs can finish writing
        tmp_dir_1.cleanup()
        tmp_dir_2.cleanup()

if __name__ == '__main__':
    unittest.main()

