/*
 * This Kotlin source file was generated by the Gradle 'init' task.
 */
package org.lightningdevkit.ldknode

import kotlin.UInt
import kotlin.test.Test
import kotlin.test.assertEquals
import kotlin.io.path.createTempDirectory
import org.junit.runner.RunWith
import org.lightningdevkit.ldknode.*;
import android.content.Context.MODE_PRIVATE
import androidx.test.core.app.ApplicationProvider
import androidx.test.ext.junit.runners.AndroidJUnit4

@RunWith(AndroidJUnit4::class)
class AndroidLibTest {
    @Test fun node_start_stop() {
        val mnemonic1 = generateEntropyMnemonic()
        val mnemonic2 = generateEntropyMnemonic()

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

        val config2 = defaultConfig()
        config2.storageDirPath = tmpDir2
        config2.listeningAddresses = listOf(listenAddress2)
        config2.network = Network.REGTEST
        config2.logLevel = LogLevel.TRACE

        val builder1 = Builder.fromMnemonic(mnemonic1, null, config1)
        val builder2 = Builder.fromMnemonic(mnemonic2, null, config2)

        val node1 = builder1.build()
        val node2 = builder2.build()

        node1.start()
        node2.start()

        val nodeId1 = node1.nodeId()
        println("Node Id 1: $nodeId1")

        val nodeId2 = node2.nodeId()
        println("Node Id 2: $nodeId2")

        val address1 = node1.newOnchainAddress()
        println("Funding address 1: $address1")

        val address2 = node2.newOnchainAddress()
        println("Funding address 2: $address2")

        node1.stop()
        node2.stop()
    }
}
