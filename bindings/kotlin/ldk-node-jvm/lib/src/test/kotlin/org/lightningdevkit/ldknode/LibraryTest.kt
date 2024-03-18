package org.lightningdevkit.ldknode

import org.junit.jupiter.api.BeforeAll
import org.junit.jupiter.api.Test
import org.junit.jupiter.api.TestInstance
import java.net.URI
import java.net.http.HttpClient
import java.net.http.HttpRequest
import java.net.http.HttpResponse
import kotlin.io.path.createTempDirectory
import kotlin.test.assertEquals

fun runCommandAndWait(vararg cmd: String): String {
    println("Running command \"${cmd.joinToString(" ")}\"")

    val processBuilder = ProcessBuilder(cmd.toList())
    val process = processBuilder.start()

    process.waitFor()
    val stdout = process.inputStream.bufferedReader().lineSequence().joinToString("\n")
    val stderr = process.errorStream.bufferedReader().lineSequence().joinToString("\n")
    return stdout + stderr
}

fun bitcoinCli(vararg cmd: String): String {
    val bitcoinCliBin = System.getenv("BITCOIN_CLI_BIN")?.split(" ") ?: listOf("bitcoin-cli")
    val bitcoinDRpcUser = System.getenv("BITCOIND_RPC_USER") ?: ""
    val bitcoinDRpcPassword = System.getenv("BITCOIND_RPC_PASSWORD") ?: ""

    val baseCommand = bitcoinCliBin + "-regtest"

    val rpcAuth = if (bitcoinDRpcUser.isNotBlank() && bitcoinDRpcPassword.isNotBlank()) {
        listOf("-rpcuser=$bitcoinDRpcUser", "-rpcpassword=$bitcoinDRpcPassword")
    } else {
        emptyList()
    }

    val fullCommand = baseCommand + rpcAuth + cmd.toList()
    return runCommandAndWait(*fullCommand.toTypedArray())
}

fun mine(blocks: UInt): String {
    val address = bitcoinCli("getnewaddress")
    val output = bitcoinCli("generatetoaddress", blocks.toString(), address)
    println("Mining output: $output")
    val re = Regex("\n.+\n\\]$")
    val lastBlock = re.find(output)!!.value.replace("]", "").replace("\"", "").replace("\n", "").trim()
    println("Last block: $lastBlock")
    return lastBlock
}

fun mineAndWait(esploraEndpoint: String, blocks: UInt) {
    val lastBlockHash = mine(blocks)
    waitForBlock(esploraEndpoint, lastBlockHash)
}

fun sendToAddress(address: String, amountSats: UInt): String {
    val amountBtc = amountSats.toDouble() / 100000000.0
    val output = bitcoinCli("sendtoaddress", address, amountBtc.toString())
    return output
}

fun waitForTx(esploraEndpoint: String, txid: String) {
    var esploraPickedUpTx = false
    val re = Regex("\"txid\":\"$txid\"")
    while (!esploraPickedUpTx) {
        val client = HttpClient.newBuilder().build()
        val request = HttpRequest.newBuilder()
            .uri(URI.create(esploraEndpoint + "/tx/" + txid))
            .build()

        val response = client.send(request, HttpResponse.BodyHandlers.ofString())

        esploraPickedUpTx = re.containsMatchIn(response.body())
        Thread.sleep(500)
    }
}

fun waitForBlock(esploraEndpoint: String, blockHash: String) {
    var esploraPickedUpBlock = false
    val re = Regex("\"in_best_chain\":true")
    while (!esploraPickedUpBlock) {
        val client = HttpClient.newBuilder().build()
        val request = HttpRequest.newBuilder()
            .uri(URI.create(esploraEndpoint + "/block/" + blockHash + "/status"))
            .build()

        val response = client.send(request, HttpResponse.BodyHandlers.ofString())

        esploraPickedUpBlock = re.containsMatchIn(response.body())
        Thread.sleep(500)
    }
}

@TestInstance(TestInstance.Lifecycle.PER_CLASS)
class LibraryTest {

    val esploraEndpoint = System.getenv("ESPLORA_ENDPOINT")

    @BeforeAll
    fun setup() {
        bitcoinCli("createwallet", "ldk_node_test")
        bitcoinCli("loadwallet", "ldk_node_test", "true")
        mine(101u)
        Thread.sleep(5_000)
    }

    @Test fun fullCycle() {
        val tmpDir1 = createTempDirectory("ldk_node").toString()
        println("Random dir 1: $tmpDir1")
        val tmpDir2 = createTempDirectory("ldk_node").toString()
        println("Random dir 2: $tmpDir2")

        val listenAddress1 = "127.0.0.1:2323"
        val listenAddress2 = "127.0.0.1:2324"

        val config1 = defaultConfig()
        config1.storageDirPath = tmpDir1
        config1.listeningAddresses = listOf(listenAddress1)
        config1.network = Network.REGTEST
        config1.logLevel = LogLevel.TRACE

        println("Config 1: $config1")

        val config2 = defaultConfig()
        config2.storageDirPath = tmpDir2
        config2.listeningAddresses = listOf(listenAddress2)
        config2.network = Network.REGTEST
        config2.logLevel = LogLevel.TRACE
        println("Config 2: $config2")

        val builder1 = Builder.fromConfig(config1)
        builder1.setEsploraServer(esploraEndpoint)
        val builder2 = Builder.fromConfig(config2)
        builder2.setEsploraServer(esploraEndpoint)

        val node1 = builder1.build()
        val node2 = builder2.build()

        node1.start()
        node2.start()

        val nodeId1 = node1.nodeId()
        println("Node Id 1: $nodeId1")

        val nodeId2 = node2.nodeId()
        println("Node Id 2: $nodeId2")

        val address1 = node1.onchainPayment().newAddress()
        println("Funding address 1: $address1")

        val address2 = node2.onchainPayment().newAddress()
        println("Funding address 2: $address2")

        val txid1 = sendToAddress(address1, 100000u)
        val txid2 = sendToAddress(address2, 100000u)
        mineAndWait(esploraEndpoint, 6u)

        waitForTx(esploraEndpoint, txid1)
        waitForTx(esploraEndpoint, txid2)

        node1.syncWallets()
        node2.syncWallets()

        val spendableBalance1 = node1.listBalances().spendableOnchainBalanceSats
        val spendableBalance2 = node2.listBalances().spendableOnchainBalanceSats
        val totalBalance1 = node1.listBalances().totalOnchainBalanceSats
        val totalBalance2 = node2.listBalances().totalOnchainBalanceSats
        println("Spendable balance 1: $spendableBalance1")
        println("Spendable balance 2: $spendableBalance1")
        println("Total balance 1: $totalBalance1")
        println("Total balance 2: $totalBalance1")
        assertEquals(100000uL, spendableBalance1)
        assertEquals(100000uL, spendableBalance2)
        assertEquals(100000uL, totalBalance1)
        assertEquals(100000uL, totalBalance2)

        node1.connectOpenChannel(nodeId2, listenAddress2, 50000u, null, null, true)

        val channelPendingEvent1 = node1.waitNextEvent()
        println("Got event: $channelPendingEvent1")
        assert(channelPendingEvent1 is Event.ChannelPending)
        node1.eventHandled()

        val channelPendingEvent2 = node2.waitNextEvent()
        println("Got event: $channelPendingEvent2")
        assert(channelPendingEvent2 is Event.ChannelPending)
        node2.eventHandled()

        val fundingTxid = when (channelPendingEvent1) {
            is Event.ChannelPending -> channelPendingEvent1.fundingTxo.txid
            else -> return
        }

        waitForTx(esploraEndpoint, fundingTxid)

        mineAndWait(esploraEndpoint, 6u)

        node1.syncWallets()
        node2.syncWallets()

        val spendableBalance1AfterOpen = node1.listBalances().spendableOnchainBalanceSats
        val spendableBalance2AfterOpen = node2.listBalances().spendableOnchainBalanceSats
        println("Spendable balance 1 after open: $spendableBalance1AfterOpen")
        println("Spendable balance 2 after open: $spendableBalance2AfterOpen")
        assert(spendableBalance1AfterOpen > 24000u)
        assert(spendableBalance1AfterOpen < 25000u)
        assertEquals(75000uL, spendableBalance2AfterOpen)

        val channelReadyEvent1 = node1.waitNextEvent()
        println("Got event: $channelReadyEvent1")
        assert(channelReadyEvent1 is Event.ChannelReady)
        node1.eventHandled()

        val channelReadyEvent2 = node2.waitNextEvent()
        println("Got event: $channelReadyEvent2")
        assert(channelReadyEvent2 is Event.ChannelReady)
        node2.eventHandled()

        val userChannelId = when (channelReadyEvent2) {
            is Event.ChannelReady -> channelReadyEvent2.userChannelId
            else -> return
        }

        val invoice = node2.bolt11Payment().receive(2500000u, "asdf", 9217u)

        node1.bolt11Payment().send(invoice)

        val paymentSuccessfulEvent = node1.waitNextEvent()
        println("Got event: $paymentSuccessfulEvent")
        assert(paymentSuccessfulEvent is Event.PaymentSuccessful)
        node1.eventHandled()

        val paymentReceivedEvent = node2.waitNextEvent()
        println("Got event: $paymentReceivedEvent")
        assert(paymentReceivedEvent is Event.PaymentReceived)
        node2.eventHandled()

        assert(node1.listPayments().size == 1)
        assert(node2.listPayments().size == 1)

        node2.closeChannel(userChannelId, nodeId1, false)

        val channelClosedEvent1 = node1.waitNextEvent()
        println("Got event: $channelClosedEvent1")
        assert(channelClosedEvent1 is Event.ChannelClosed)
        node1.eventHandled()

        val channelClosedEvent2 = node2.waitNextEvent()
        println("Got event: $channelClosedEvent2")
        assert(channelClosedEvent2 is Event.ChannelClosed)
        node2.eventHandled()

        mineAndWait(esploraEndpoint, 1u)

        node1.syncWallets()
        node2.syncWallets()

        val spendableBalance1AfterClose = node1.listBalances().spendableOnchainBalanceSats
        val spendableBalance2AfterClose = node2.listBalances().spendableOnchainBalanceSats
        println("Spendable balance 1 after close: $spendableBalance1AfterClose")
        println("Spendable balance 2 after close: $spendableBalance2AfterClose")
        assert(spendableBalance1AfterClose > 95000u)
        assert(spendableBalance1AfterClose < 100000u)
        assertEquals(102500uL, spendableBalance2AfterClose)

        node1.stop()
        node2.stop()
    }
}
