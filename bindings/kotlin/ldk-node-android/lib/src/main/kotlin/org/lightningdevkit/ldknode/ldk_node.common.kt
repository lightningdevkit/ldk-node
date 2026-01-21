

@file:Suppress("RemoveRedundantBackticks")

package org.lightningdevkit.ldknode

// Common helper code.
//
// Ideally this would live in a separate .kt file where it can be unittested etc
// in isolation, and perhaps even published as a re-useable package.
//
// However, it's important that the details of how this helper code works (e.g. the
// way that different builtin types are passed across the FFI) exactly match what's
// expected by the Rust code on the other side of the interface. In practice right
// now that means coming from the exact some version of `uniffi` that was used to
// compile the Rust component. The easiest way to ensure this is to bundle the Kotlin
// helpers directly inline like we're doing here.

class InternalException(message: String) : kotlin.Exception(message)

// Public interface members begin here.


// Interface implemented by anything that can contain an object reference.
//
// Such types expose a `destroy()` method that must be called to cleanly
// dispose of the contained objects. Failure to call this method may result
// in memory leaks.
//
// The easiest way to ensure this method is called is to use the `.use`
// helper method to execute a block and destroy the object at the end.
@OptIn(ExperimentalStdlibApi::class)
interface Disposable : AutoCloseable {
    fun destroy()
    override fun close() = destroy()
    companion object {
        internal fun destroy(vararg args: Any?) {
            for (arg in args) {
                when (arg) {
                    is Disposable -> arg.destroy()
                    is Iterable<*> -> {
                        for (element in arg) {
                            if (element is Disposable) {
                                element.destroy()
                            }
                        }
                    }
                    is Map<*, *> -> {
                        for (element in arg.values) {
                            if (element is Disposable) {
                                element.destroy()
                            }
                        }
                    }
                    is Array<*> -> {
                        for (element in arg) {
                            if (element is Disposable) {
                                element.destroy()
                            }
                        }
                    }
                }
            }
        }
    }
}

@OptIn(kotlin.contracts.ExperimentalContracts::class)
inline fun <T : Disposable?, R> T.use(block: (T) -> R): R {
    kotlin.contracts.contract {
        callsInPlace(block, kotlin.contracts.InvocationKind.EXACTLY_ONCE)
    }
    return try {
        block(this)
    } finally {
        try {
            // N.B. our implementation is on the nullable type `Disposable?`.
            this?.destroy()
        } catch (e: Throwable) {
            // swallow
        }
    }
}

/** Used to instantiate an interface without an actual pointer, for fakes in tests, mostly. */
object NoPointer



















interface Bolt11InvoiceInterface {
    
    fun `amountMilliSatoshis`(): kotlin.ULong?
    
    fun `currency`(): Currency
    
    fun `expiryTimeSeconds`(): kotlin.ULong
    
    fun `fallbackAddresses`(): List<Address>
    
    fun `invoiceDescription`(): Bolt11InvoiceDescription
    
    fun `isExpired`(): kotlin.Boolean
    
    fun `minFinalCltvExpiryDelta`(): kotlin.ULong
    
    fun `network`(): Network
    
    fun `paymentHash`(): PaymentHash
    
    fun `paymentSecret`(): PaymentSecret
    
    fun `recoverPayeePubKey`(): PublicKey
    
    fun `routeHints`(): List<List<RouteHintHop>>
    
    fun `secondsSinceEpoch`(): kotlin.ULong
    
    fun `secondsUntilExpiry`(): kotlin.ULong
    
    fun `signableHash`(): List<kotlin.UByte>
    
    fun `wouldExpire`(`atTimeSeconds`: kotlin.ULong): kotlin.Boolean
    
    companion object
}




interface Bolt11PaymentInterface {
    
    @Throws(NodeException::class)
    fun `claimForHash`(`paymentHash`: PaymentHash, `claimableAmountMsat`: kotlin.ULong, `preimage`: PaymentPreimage)
    
    @Throws(NodeException::class)
    fun `estimateRoutingFees`(`invoice`: Bolt11Invoice): kotlin.ULong
    
    @Throws(NodeException::class)
    fun `estimateRoutingFeesUsingAmount`(`invoice`: Bolt11Invoice, `amountMsat`: kotlin.ULong): kotlin.ULong
    
    @Throws(NodeException::class)
    fun `failForHash`(`paymentHash`: PaymentHash)
    
    @Throws(NodeException::class)
    fun `receive`(`amountMsat`: kotlin.ULong, `description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt): Bolt11Invoice
    
    @Throws(NodeException::class)
    fun `receiveForHash`(`amountMsat`: kotlin.ULong, `description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `paymentHash`: PaymentHash): Bolt11Invoice
    
    @Throws(NodeException::class)
    fun `receiveVariableAmount`(`description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt): Bolt11Invoice
    
    @Throws(NodeException::class)
    fun `receiveVariableAmountForHash`(`description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `paymentHash`: PaymentHash): Bolt11Invoice
    
    @Throws(NodeException::class)
    fun `receiveVariableAmountViaJitChannel`(`description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `maxProportionalLspFeeLimitPpmMsat`: kotlin.ULong?): Bolt11Invoice
    
    @Throws(NodeException::class)
    fun `receiveVariableAmountViaJitChannelForHash`(`description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `maxProportionalLspFeeLimitPpmMsat`: kotlin.ULong?, `paymentHash`: PaymentHash): Bolt11Invoice
    
    @Throws(NodeException::class)
    fun `receiveViaJitChannel`(`amountMsat`: kotlin.ULong, `description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `maxLspFeeLimitMsat`: kotlin.ULong?): Bolt11Invoice
    
    @Throws(NodeException::class)
    fun `receiveViaJitChannelForHash`(`amountMsat`: kotlin.ULong, `description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `maxLspFeeLimitMsat`: kotlin.ULong?, `paymentHash`: PaymentHash): Bolt11Invoice
    
    @Throws(NodeException::class)
    fun `send`(`invoice`: Bolt11Invoice, `routeParameters`: RouteParametersConfig?): PaymentId
    
    @Throws(NodeException::class)
    fun `sendProbes`(`invoice`: Bolt11Invoice, `routeParameters`: RouteParametersConfig?)
    
    @Throws(NodeException::class)
    fun `sendProbesUsingAmount`(`invoice`: Bolt11Invoice, `amountMsat`: kotlin.ULong, `routeParameters`: RouteParametersConfig?)
    
    @Throws(NodeException::class)
    fun `sendUsingAmount`(`invoice`: Bolt11Invoice, `amountMsat`: kotlin.ULong, `routeParameters`: RouteParametersConfig?): PaymentId
    
    companion object
}




interface Bolt12InvoiceInterface {
    
    fun `absoluteExpirySeconds`(): kotlin.ULong?
    
    fun `amount`(): OfferAmount?
    
    fun `amountMsats`(): kotlin.ULong
    
    fun `chain`(): List<kotlin.UByte>
    
    fun `createdAt`(): kotlin.ULong
    
    fun `encode`(): List<kotlin.UByte>
    
    fun `fallbackAddresses`(): List<Address>
    
    fun `invoiceDescription`(): kotlin.String?
    
    fun `isExpired`(): kotlin.Boolean
    
    fun `issuer`(): kotlin.String?
    
    fun `issuerSigningPubkey`(): PublicKey?
    
    fun `metadata`(): List<kotlin.UByte>?
    
    fun `offerChains`(): List<List<kotlin.UByte>>?
    
    fun `payerNote`(): kotlin.String?
    
    fun `payerSigningPubkey`(): PublicKey
    
    fun `paymentHash`(): PaymentHash
    
    fun `quantity`(): kotlin.ULong?
    
    fun `relativeExpiry`(): kotlin.ULong
    
    fun `signableHash`(): List<kotlin.UByte>
    
    fun `signingPubkey`(): PublicKey
    
    companion object
}




interface Bolt12PaymentInterface {
    
    @Throws(NodeException::class)
    fun `blindedPathsForAsyncRecipient`(`recipientId`: kotlin.ByteArray): kotlin.ByteArray
    
    @Throws(NodeException::class)
    fun `initiateRefund`(`amountMsat`: kotlin.ULong, `expirySecs`: kotlin.UInt, `quantity`: kotlin.ULong?, `payerNote`: kotlin.String?, `routeParameters`: RouteParametersConfig?): Refund
    
    @Throws(NodeException::class)
    fun `receive`(`amountMsat`: kotlin.ULong, `description`: kotlin.String, `expirySecs`: kotlin.UInt?, `quantity`: kotlin.ULong?): Offer
    
    @Throws(NodeException::class)
    fun `receiveAsync`(): Offer
    
    @Throws(NodeException::class)
    fun `receiveVariableAmount`(`description`: kotlin.String, `expirySecs`: kotlin.UInt?): Offer
    
    @Throws(NodeException::class)
    fun `requestRefundPayment`(`refund`: Refund): Bolt12Invoice
    
    @Throws(NodeException::class)
    fun `send`(`offer`: Offer, `quantity`: kotlin.ULong?, `payerNote`: kotlin.String?, `routeParameters`: RouteParametersConfig?): PaymentId
    
    @Throws(NodeException::class)
    fun `sendUsingAmount`(`offer`: Offer, `amountMsat`: kotlin.ULong, `quantity`: kotlin.ULong?, `payerNote`: kotlin.String?, `routeParameters`: RouteParametersConfig?): PaymentId
    
    @Throws(NodeException::class)
    fun `setPathsToStaticInvoiceServer`(`paths`: kotlin.ByteArray)
    
    companion object
}




interface BuilderInterface {
    
    @Throws(BuildException::class)
    fun `build`(): Node
    
    @Throws(BuildException::class)
    fun `buildWithFsStore`(): Node
    
    @Throws(BuildException::class)
    fun `buildWithVssStore`(`vssUrl`: kotlin.String, `storeId`: kotlin.String, `lnurlAuthServerUrl`: kotlin.String, `fixedHeaders`: Map<kotlin.String, kotlin.String>): Node
    
    @Throws(BuildException::class)
    fun `buildWithVssStoreAndFixedHeaders`(`vssUrl`: kotlin.String, `storeId`: kotlin.String, `fixedHeaders`: Map<kotlin.String, kotlin.String>): Node
    
    @Throws(BuildException::class)
    fun `buildWithVssStoreAndHeaderProvider`(`vssUrl`: kotlin.String, `storeId`: kotlin.String, `headerProvider`: VssHeaderProvider): Node
    
    @Throws(BuildException::class)
    fun `setAnnouncementAddresses`(`announcementAddresses`: List<SocketAddress>)
    
    @Throws(BuildException::class)
    fun `setAsyncPaymentsRole`(`role`: AsyncPaymentsRole?)
    
    fun `setChainSourceBitcoindRest`(`restHost`: kotlin.String, `restPort`: kotlin.UShort, `rpcHost`: kotlin.String, `rpcPort`: kotlin.UShort, `rpcUser`: kotlin.String, `rpcPassword`: kotlin.String)
    
    fun `setChainSourceBitcoindRpc`(`rpcHost`: kotlin.String, `rpcPort`: kotlin.UShort, `rpcUser`: kotlin.String, `rpcPassword`: kotlin.String)
    
    fun `setChainSourceElectrum`(`serverUrl`: kotlin.String, `config`: ElectrumSyncConfig?)
    
    fun `setChainSourceEsplora`(`serverUrl`: kotlin.String, `config`: EsploraSyncConfig?)
    
    fun `setChannelDataMigration`(`migration`: ChannelDataMigration)
    
    fun `setCustomLogger`(`logWriter`: LogWriter)
    
    fun `setEntropyBip39Mnemonic`(`mnemonic`: Mnemonic, `passphrase`: kotlin.String?)
    
    @Throws(BuildException::class)
    fun `setEntropySeedBytes`(`seedBytes`: List<kotlin.UByte>)
    
    fun `setEntropySeedPath`(`seedPath`: kotlin.String)
    
    fun `setFilesystemLogger`(`logFilePath`: kotlin.String?, `maxLogLevel`: LogLevel?)
    
    fun `setGossipSourceP2p`()
    
    fun `setGossipSourceRgs`(`rgsServerUrl`: kotlin.String)
    
    fun `setLiquiditySourceLsps1`(`nodeId`: PublicKey, `address`: SocketAddress, `token`: kotlin.String?)
    
    fun `setLiquiditySourceLsps2`(`nodeId`: PublicKey, `address`: SocketAddress, `token`: kotlin.String?)
    
    @Throws(BuildException::class)
    fun `setListeningAddresses`(`listeningAddresses`: List<SocketAddress>)
    
    fun `setLogFacadeLogger`()
    
    fun `setNetwork`(`network`: Network)
    
    @Throws(BuildException::class)
    fun `setNodeAlias`(`nodeAlias`: kotlin.String)
    
    fun `setPathfindingScoresSource`(`url`: kotlin.String)
    
    fun `setStorageDirPath`(`storageDirPath`: kotlin.String)
    
    companion object
}




interface FeeRateInterface {
    
    fun `toSatPerKwu`(): kotlin.ULong
    
    fun `toSatPerVbCeil`(): kotlin.ULong
    
    fun `toSatPerVbFloor`(): kotlin.ULong
    
    companion object
}




interface Lsps1LiquidityInterface {
    
    @Throws(NodeException::class)
    fun `checkOrderStatus`(`orderId`: Lsps1OrderId): Lsps1OrderStatus
    
    @Throws(NodeException::class)
    fun `requestChannel`(`lspBalanceSat`: kotlin.ULong, `clientBalanceSat`: kotlin.ULong, `channelExpiryBlocks`: kotlin.UInt, `announceChannel`: kotlin.Boolean): Lsps1OrderStatus
    
    companion object
}




interface LogWriter {
    
    fun `log`(`record`: LogRecord)
    
    companion object
}




interface NetworkGraphInterface {
    
    fun `channel`(`shortChannelId`: kotlin.ULong): ChannelInfo?
    
    fun `listChannels`(): List<kotlin.ULong>
    
    fun `listNodes`(): List<NodeId>
    
    fun `node`(`nodeId`: NodeId): NodeInfo?
    
    companion object
}




interface NodeInterface {
    
    fun `announcementAddresses`(): List<SocketAddress>?
    
    fun `bolt11Payment`(): Bolt11Payment
    
    fun `bolt12Payment`(): Bolt12Payment
    
    @Throws(NodeException::class)
    fun `closeChannel`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey)
    
    fun `config`(): Config
    
    @Throws(NodeException::class)
    fun `connect`(`nodeId`: PublicKey, `address`: SocketAddress, `persist`: kotlin.Boolean)
    
    fun `currentSyncIntervals`(): RuntimeSyncIntervals
    
    @Throws(NodeException::class)
    fun `disconnect`(`nodeId`: PublicKey)
    
    @Throws(NodeException::class)
    fun `eventHandled`()
    
    @Throws(NodeException::class)
    fun `exportPathfindingScores`(): kotlin.ByteArray
    
    @Throws(NodeException::class)
    fun `forceCloseChannel`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey, `reason`: kotlin.String?)
    
    @Throws(NodeException::class)
    fun `getAddressBalance`(`addressStr`: kotlin.String): kotlin.ULong
    
    fun `getTransactionDetails`(`txid`: Txid): TransactionDetails?
    
    fun `listBalances`(): BalanceDetails
    
    fun `listChannels`(): List<ChannelDetails>
    
    fun `listPayments`(): List<PaymentDetails>
    
    fun `listPeers`(): List<PeerDetails>
    
    fun `listeningAddresses`(): List<SocketAddress>?
    
    fun `lsps1Liquidity`(): Lsps1Liquidity
    
    fun `networkGraph`(): NetworkGraph
    
    fun `nextEvent`(): Event?
    
    suspend fun `nextEventAsync`(): Event
    
    fun `nodeAlias`(): NodeAlias?
    
    fun `nodeId`(): PublicKey
    
    fun `onchainPayment`(): OnchainPayment
    
    @Throws(NodeException::class)
    fun `openAnnouncedChannel`(`nodeId`: PublicKey, `address`: SocketAddress, `channelAmountSats`: kotlin.ULong, `pushToCounterpartyMsat`: kotlin.ULong?, `channelConfig`: ChannelConfig?): UserChannelId
    
    @Throws(NodeException::class)
    fun `openChannel`(`nodeId`: PublicKey, `address`: SocketAddress, `channelAmountSats`: kotlin.ULong, `pushToCounterpartyMsat`: kotlin.ULong?, `channelConfig`: ChannelConfig?): UserChannelId
    
    fun `payment`(`paymentId`: PaymentId): PaymentDetails?
    
    @Throws(NodeException::class)
    fun `removePayment`(`paymentId`: PaymentId)
    
    fun `signMessage`(`msg`: List<kotlin.UByte>): kotlin.String
    
    @Throws(NodeException::class)
    fun `spliceIn`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey, `spliceAmountSats`: kotlin.ULong)
    
    @Throws(NodeException::class)
    fun `spliceOut`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey, `address`: Address, `spliceAmountSats`: kotlin.ULong)
    
    fun `spontaneousPayment`(): SpontaneousPayment
    
    @Throws(NodeException::class)
    fun `start`()
    
    fun `status`(): NodeStatus
    
    @Throws(NodeException::class)
    fun `stop`()
    
    @Throws(NodeException::class)
    fun `syncWallets`()
    
    fun `unifiedQrPayment`(): UnifiedQrPayment
    
    @Throws(NodeException::class)
    fun `updateChannelConfig`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey, `channelConfig`: ChannelConfig)
    
    @Throws(NodeException::class)
    fun `updateSyncIntervals`(`intervals`: RuntimeSyncIntervals)
    
    fun `verifySignature`(`msg`: List<kotlin.UByte>, `sig`: kotlin.String, `pkey`: PublicKey): kotlin.Boolean
    
    fun `waitNextEvent`(): Event
    
    companion object
}




interface OfferInterface {
    
    fun `absoluteExpirySeconds`(): kotlin.ULong?
    
    fun `amount`(): OfferAmount?
    
    fun `chains`(): List<Network>
    
    fun `expectsQuantity`(): kotlin.Boolean
    
    fun `id`(): OfferId
    
    fun `isExpired`(): kotlin.Boolean
    
    fun `isValidQuantity`(`quantity`: kotlin.ULong): kotlin.Boolean
    
    fun `issuer`(): kotlin.String?
    
    fun `issuerSigningPubkey`(): PublicKey?
    
    fun `metadata`(): List<kotlin.UByte>?
    
    fun `offerDescription`(): kotlin.String?
    
    fun `supportsChain`(`chain`: Network): kotlin.Boolean
    
    companion object
}




interface OnchainPaymentInterface {
    
    @Throws(NodeException::class)
    fun `accelerateByCpfp`(`txid`: Txid, `feeRate`: FeeRate?, `destinationAddress`: Address?): Txid
    
    @Throws(NodeException::class)
    fun `bumpFeeByRbf`(`txid`: Txid, `feeRate`: FeeRate): Txid
    
    @Throws(NodeException::class)
    fun `calculateCpfpFeeRate`(`parentTxid`: Txid, `urgent`: kotlin.Boolean): FeeRate
    
    @Throws(NodeException::class)
    fun `calculateTotalFee`(`address`: Address, `amountSats`: kotlin.ULong, `feeRate`: FeeRate?, `utxosToSpend`: List<SpendableUtxo>?): kotlin.ULong
    
    @Throws(NodeException::class)
    fun `listSpendableOutputs`(): List<SpendableUtxo>
    
    @Throws(NodeException::class)
    fun `newAddress`(): Address
    
    @Throws(NodeException::class)
    fun `selectUtxosWithAlgorithm`(`targetAmountSats`: kotlin.ULong, `feeRate`: FeeRate?, `algorithm`: CoinSelectionAlgorithm, `utxos`: List<SpendableUtxo>?): List<SpendableUtxo>
    
    @Throws(NodeException::class)
    fun `sendAllToAddress`(`address`: Address, `retainReserve`: kotlin.Boolean, `feeRate`: FeeRate?): Txid
    
    @Throws(NodeException::class)
    fun `sendToAddress`(`address`: Address, `amountSats`: kotlin.ULong, `feeRate`: FeeRate?, `utxosToSpend`: List<SpendableUtxo>?): Txid
    
    companion object
}




interface RefundInterface {
    
    fun `absoluteExpirySeconds`(): kotlin.ULong?
    
    fun `amountMsats`(): kotlin.ULong
    
    fun `chain`(): Network?
    
    fun `isExpired`(): kotlin.Boolean
    
    fun `issuer`(): kotlin.String?
    
    fun `payerMetadata`(): List<kotlin.UByte>
    
    fun `payerNote`(): kotlin.String?
    
    fun `payerSigningPubkey`(): PublicKey
    
    fun `quantity`(): kotlin.ULong?
    
    fun `refundDescription`(): kotlin.String
    
    companion object
}




interface SpontaneousPaymentInterface {
    
    @Throws(NodeException::class)
    fun `send`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey, `routeParameters`: RouteParametersConfig?): PaymentId
    
    @Throws(NodeException::class)
    fun `sendProbes`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey)
    
    @Throws(NodeException::class)
    fun `sendWithCustomTlvs`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey, `routeParameters`: RouteParametersConfig?, `customTlvs`: List<CustomTlvRecord>): PaymentId
    
    @Throws(NodeException::class)
    fun `sendWithPreimage`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey, `preimage`: PaymentPreimage, `routeParameters`: RouteParametersConfig?): PaymentId
    
    @Throws(NodeException::class)
    fun `sendWithPreimageAndCustomTlvs`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey, `customTlvs`: List<CustomTlvRecord>, `preimage`: PaymentPreimage, `routeParameters`: RouteParametersConfig?): PaymentId
    
    companion object
}




interface UnifiedQrPaymentInterface {
    
    @Throws(NodeException::class)
    fun `receive`(`amountSats`: kotlin.ULong, `message`: kotlin.String, `expirySec`: kotlin.UInt): kotlin.String
    
    @Throws(NodeException::class)
    fun `send`(`uriStr`: kotlin.String, `routeParameters`: RouteParametersConfig?): QrPaymentResult
    
    companion object
}




interface VssHeaderProviderInterface {
    
    @Throws(VssHeaderProviderException::class, kotlin.coroutines.cancellation.CancellationException::class)
    suspend fun `getHeaders`(`request`: List<kotlin.UByte>): Map<kotlin.String, kotlin.String>
    
    companion object
}




@kotlinx.serialization.Serializable
data class AnchorChannelsConfig (
    val `trustedPeersNoReserve`: List<PublicKey>, 
    val `perChannelReserveSats`: kotlin.ULong
) {
    companion object
}



@kotlinx.serialization.Serializable
data class BackgroundSyncConfig (
    val `onchainWalletSyncIntervalSecs`: kotlin.ULong, 
    val `lightningWalletSyncIntervalSecs`: kotlin.ULong, 
    val `feeRateCacheUpdateIntervalSecs`: kotlin.ULong
) {
    companion object
}



@kotlinx.serialization.Serializable
data class BalanceDetails (
    val `totalOnchainBalanceSats`: kotlin.ULong, 
    val `spendableOnchainBalanceSats`: kotlin.ULong, 
    val `totalAnchorChannelsReserveSats`: kotlin.ULong, 
    val `totalLightningBalanceSats`: kotlin.ULong, 
    val `lightningBalances`: List<LightningBalance>, 
    val `pendingBalancesFromChannelClosures`: List<PendingSweepBalance>
) {
    companion object
}



@kotlinx.serialization.Serializable
data class BestBlock (
    val `blockHash`: BlockHash, 
    val `height`: kotlin.UInt
) {
    companion object
}



@kotlinx.serialization.Serializable
data class ChannelConfig (
    val `forwardingFeeProportionalMillionths`: kotlin.UInt, 
    val `forwardingFeeBaseMsat`: kotlin.UInt, 
    val `cltvExpiryDelta`: kotlin.UShort, 
    val `maxDustHtlcExposure`: MaxDustHtlcExposure, 
    val `forceCloseAvoidanceMaxFeeSatoshis`: kotlin.ULong, 
    val `acceptUnderpayingHtlcs`: kotlin.Boolean
) {
    companion object
}



@kotlinx.serialization.Serializable
data class ChannelDataMigration (
    val `channelManager`: List<kotlin.UByte>?, 
    val `channelMonitors`: List<List<kotlin.UByte>>
) {
    companion object
}



@kotlinx.serialization.Serializable
data class ChannelDetails (
    val `channelId`: ChannelId, 
    val `counterpartyNodeId`: PublicKey, 
    val `fundingTxo`: OutPoint?, 
    val `shortChannelId`: kotlin.ULong?, 
    val `outboundScidAlias`: kotlin.ULong?, 
    val `inboundScidAlias`: kotlin.ULong?, 
    val `channelValueSats`: kotlin.ULong, 
    val `unspendablePunishmentReserve`: kotlin.ULong?, 
    val `userChannelId`: UserChannelId, 
    val `feerateSatPer1000Weight`: kotlin.UInt, 
    val `outboundCapacityMsat`: kotlin.ULong, 
    val `inboundCapacityMsat`: kotlin.ULong, 
    val `confirmationsRequired`: kotlin.UInt?, 
    val `confirmations`: kotlin.UInt?, 
    val `isOutbound`: kotlin.Boolean, 
    val `isChannelReady`: kotlin.Boolean, 
    val `isUsable`: kotlin.Boolean, 
    val `isAnnounced`: kotlin.Boolean, 
    val `cltvExpiryDelta`: kotlin.UShort?, 
    val `counterpartyUnspendablePunishmentReserve`: kotlin.ULong, 
    val `counterpartyOutboundHtlcMinimumMsat`: kotlin.ULong?, 
    val `counterpartyOutboundHtlcMaximumMsat`: kotlin.ULong?, 
    val `counterpartyForwardingInfoFeeBaseMsat`: kotlin.UInt?, 
    val `counterpartyForwardingInfoFeeProportionalMillionths`: kotlin.UInt?, 
    val `counterpartyForwardingInfoCltvExpiryDelta`: kotlin.UShort?, 
    val `nextOutboundHtlcLimitMsat`: kotlin.ULong, 
    val `nextOutboundHtlcMinimumMsat`: kotlin.ULong, 
    val `forceCloseSpendDelay`: kotlin.UShort?, 
    val `inboundHtlcMinimumMsat`: kotlin.ULong, 
    val `inboundHtlcMaximumMsat`: kotlin.ULong?, 
    val `config`: ChannelConfig, 
    val `claimableOnCloseSats`: kotlin.ULong?
) {
    companion object
}



@kotlinx.serialization.Serializable
data class ChannelInfo (
    val `nodeOne`: NodeId, 
    val `oneToTwo`: ChannelUpdateInfo?, 
    val `nodeTwo`: NodeId, 
    val `twoToOne`: ChannelUpdateInfo?, 
    val `capacitySats`: kotlin.ULong?
) {
    companion object
}



@kotlinx.serialization.Serializable
data class ChannelUpdateInfo (
    val `lastUpdate`: kotlin.UInt, 
    val `enabled`: kotlin.Boolean, 
    val `cltvExpiryDelta`: kotlin.UShort, 
    val `htlcMinimumMsat`: kotlin.ULong, 
    val `htlcMaximumMsat`: kotlin.ULong, 
    val `fees`: RoutingFees
) {
    companion object
}



@kotlinx.serialization.Serializable
data class Config (
    val `storageDirPath`: kotlin.String, 
    val `network`: Network, 
    val `listeningAddresses`: List<SocketAddress>?, 
    val `announcementAddresses`: List<SocketAddress>?, 
    val `nodeAlias`: NodeAlias?, 
    val `trustedPeers0conf`: List<PublicKey>, 
    val `probingLiquidityLimitMultiplier`: kotlin.ULong, 
    val `anchorChannelsConfig`: AnchorChannelsConfig?, 
    val `routeParameters`: RouteParametersConfig?, 
    val `includeUntrustedPendingInSpendable`: kotlin.Boolean
) {
    companion object
}



@kotlinx.serialization.Serializable
data class CustomTlvRecord (
    val `typeNum`: kotlin.ULong, 
    val `value`: List<kotlin.UByte>
) {
    companion object
}



@kotlinx.serialization.Serializable
data class ElectrumSyncConfig (
    val `backgroundSyncConfig`: BackgroundSyncConfig?
) {
    companion object
}



@kotlinx.serialization.Serializable
data class EsploraSyncConfig (
    val `backgroundSyncConfig`: BackgroundSyncConfig?
) {
    companion object
}



@kotlinx.serialization.Serializable
data class LspFeeLimits (
    val `maxTotalOpeningFeeMsat`: kotlin.ULong?, 
    val `maxProportionalOpeningFeePpmMsat`: kotlin.ULong?
) {
    companion object
}




data class Lsps1Bolt11PaymentInfo (
    val `state`: Lsps1PaymentState, 
    val `expiresAt`: LspsDateTime, 
    val `feeTotalSat`: kotlin.ULong, 
    val `orderTotalSat`: kotlin.ULong, 
    val `invoice`: Bolt11Invoice
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`state`,
            this.`expiresAt`,
            this.`feeTotalSat`,
            this.`orderTotalSat`,
            this.`invoice`,
        )
    }
    companion object
}



@kotlinx.serialization.Serializable
data class Lsps1ChannelInfo (
    val `fundedAt`: LspsDateTime, 
    val `fundingOutpoint`: OutPoint, 
    val `expiresAt`: LspsDateTime
) {
    companion object
}




data class Lsps1OnchainPaymentInfo (
    val `state`: Lsps1PaymentState, 
    val `expiresAt`: LspsDateTime, 
    val `feeTotalSat`: kotlin.ULong, 
    val `orderTotalSat`: kotlin.ULong, 
    val `address`: Address, 
    val `minOnchainPaymentConfirmations`: kotlin.UShort?, 
    val `minFeeFor0conf`: FeeRate, 
    val `refundOnchainAddress`: Address?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`state`,
            this.`expiresAt`,
            this.`feeTotalSat`,
            this.`orderTotalSat`,
            this.`address`,
            this.`minOnchainPaymentConfirmations`,
            this.`minFeeFor0conf`,
            this.`refundOnchainAddress`,
        )
    }
    companion object
}



@kotlinx.serialization.Serializable
data class Lsps1OrderParams (
    val `lspBalanceSat`: kotlin.ULong, 
    val `clientBalanceSat`: kotlin.ULong, 
    val `requiredChannelConfirmations`: kotlin.UShort, 
    val `fundingConfirmsWithinBlocks`: kotlin.UShort, 
    val `channelExpiryBlocks`: kotlin.UInt, 
    val `token`: kotlin.String?, 
    val `announceChannel`: kotlin.Boolean
) {
    companion object
}




data class Lsps1OrderStatus (
    val `orderId`: Lsps1OrderId, 
    val `orderParams`: Lsps1OrderParams, 
    val `paymentOptions`: Lsps1PaymentInfo, 
    val `channelState`: Lsps1ChannelInfo?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`orderId`,
            this.`orderParams`,
            this.`paymentOptions`,
            this.`channelState`,
        )
    }
    companion object
}




data class Lsps1PaymentInfo (
    val `bolt11`: Lsps1Bolt11PaymentInfo?, 
    val `onchain`: Lsps1OnchainPaymentInfo?
) : Disposable {
    override fun destroy() {
        Disposable.destroy(
            this.`bolt11`,
            this.`onchain`,
        )
    }
    companion object
}



@kotlinx.serialization.Serializable
data class Lsps2ServiceConfig (
    val `requireToken`: kotlin.String?, 
    val `advertiseService`: kotlin.Boolean, 
    val `channelOpeningFeePpm`: kotlin.UInt, 
    val `channelOverProvisioningPpm`: kotlin.UInt, 
    val `minChannelOpeningFeeMsat`: kotlin.ULong, 
    val `minChannelLifetime`: kotlin.UInt, 
    val `maxClientToSelfDelay`: kotlin.UInt, 
    val `minPaymentSizeMsat`: kotlin.ULong, 
    val `maxPaymentSizeMsat`: kotlin.ULong, 
    val `clientTrustsLsp`: kotlin.Boolean
) {
    companion object
}



@kotlinx.serialization.Serializable
data class LogRecord (
    val `level`: LogLevel, 
    val `args`: kotlin.String, 
    val `modulePath`: kotlin.String, 
    val `line`: kotlin.UInt
) {
    companion object
}



@kotlinx.serialization.Serializable
data class NodeAnnouncementInfo (
    val `lastUpdate`: kotlin.UInt, 
    val `alias`: kotlin.String, 
    val `addresses`: List<SocketAddress>
) {
    companion object
}



@kotlinx.serialization.Serializable
data class NodeInfo (
    val `channels`: List<kotlin.ULong>, 
    val `announcementInfo`: NodeAnnouncementInfo?
) {
    companion object
}



@kotlinx.serialization.Serializable
data class NodeStatus (
    val `isRunning`: kotlin.Boolean, 
    val `currentBestBlock`: BestBlock, 
    val `latestLightningWalletSyncTimestamp`: kotlin.ULong?, 
    val `latestOnchainWalletSyncTimestamp`: kotlin.ULong?, 
    val `latestFeeRateCacheUpdateTimestamp`: kotlin.ULong?, 
    val `latestRgsSnapshotTimestamp`: kotlin.ULong?, 
    val `latestPathfindingScoresSyncTimestamp`: kotlin.ULong?, 
    val `latestNodeAnnouncementBroadcastTimestamp`: kotlin.ULong?, 
    val `latestChannelMonitorArchivalHeight`: kotlin.UInt?
) {
    companion object
}



@kotlinx.serialization.Serializable
data class OutPoint (
    val `txid`: Txid, 
    val `vout`: kotlin.UInt
) {
    companion object
}



@kotlinx.serialization.Serializable
data class PaymentDetails (
    val `id`: PaymentId, 
    val `kind`: PaymentKind, 
    val `amountMsat`: kotlin.ULong?, 
    val `feePaidMsat`: kotlin.ULong?, 
    val `direction`: PaymentDirection, 
    val `status`: PaymentStatus, 
    val `latestUpdateTimestamp`: kotlin.ULong
) {
    companion object
}



@kotlinx.serialization.Serializable
data class PeerDetails (
    val `nodeId`: PublicKey, 
    val `address`: SocketAddress, 
    val `isPersisted`: kotlin.Boolean, 
    val `isConnected`: kotlin.Boolean
) {
    companion object
}



@kotlinx.serialization.Serializable
data class RouteHintHop (
    val `srcNodeId`: PublicKey, 
    val `shortChannelId`: kotlin.ULong, 
    val `cltvExpiryDelta`: kotlin.UShort, 
    val `htlcMinimumMsat`: kotlin.ULong?, 
    val `htlcMaximumMsat`: kotlin.ULong?, 
    val `fees`: RoutingFees
) {
    companion object
}



@kotlinx.serialization.Serializable
data class RouteParametersConfig (
    val `maxTotalRoutingFeeMsat`: kotlin.ULong?, 
    val `maxTotalCltvExpiryDelta`: kotlin.UInt, 
    val `maxPathCount`: kotlin.UByte, 
    val `maxChannelSaturationPowerOfHalf`: kotlin.UByte
) {
    companion object
}



@kotlinx.serialization.Serializable
data class RoutingFees (
    val `baseMsat`: kotlin.UInt, 
    val `proportionalMillionths`: kotlin.UInt
) {
    companion object
}



@kotlinx.serialization.Serializable
data class RuntimeSyncIntervals (
    val `onchainWalletSyncIntervalSecs`: kotlin.ULong, 
    val `lightningWalletSyncIntervalSecs`: kotlin.ULong, 
    val `feeRateCacheUpdateIntervalSecs`: kotlin.ULong
) {
    companion object
}



@kotlinx.serialization.Serializable
data class SpendableUtxo (
    val `outpoint`: OutPoint, 
    val `valueSats`: kotlin.ULong
) {
    companion object
}



@kotlinx.serialization.Serializable
data class TransactionDetails (
    val `amountSats`: kotlin.Long, 
    val `inputs`: List<TxInput>, 
    val `outputs`: List<TxOutput>
) {
    companion object
}



@kotlinx.serialization.Serializable
data class TxInput (
    val `txid`: Txid, 
    val `vout`: kotlin.UInt, 
    val `scriptsig`: kotlin.String, 
    val `witness`: List<kotlin.String>, 
    val `sequence`: kotlin.UInt
) {
    companion object
}



@kotlinx.serialization.Serializable
data class TxOutput (
    val `scriptpubkey`: kotlin.String, 
    val `scriptpubkeyType`: kotlin.String?, 
    val `scriptpubkeyAddress`: kotlin.String?, 
    val `value`: kotlin.Long, 
    val `n`: kotlin.UInt
) {
    companion object
}





@kotlinx.serialization.Serializable
enum class AsyncPaymentsRole {
    
    CLIENT,
    SERVER;
    companion object
}







@kotlinx.serialization.Serializable
enum class BalanceSource {
    
    HOLDER_FORCE_CLOSED,
    COUNTERPARTY_FORCE_CLOSED,
    COOP_CLOSE,
    HTLC;
    companion object
}






@kotlinx.serialization.Serializable
sealed class Bolt11InvoiceDescription {
    @kotlinx.serialization.Serializable
    data class Hash(
        val `hash`: kotlin.String,
    ) : Bolt11InvoiceDescription() {
    }
    @kotlinx.serialization.Serializable
    data class Direct(
        val `description`: kotlin.String,
    ) : Bolt11InvoiceDescription() {
    }
    
}







sealed class BuildException(message: String): kotlin.Exception(message) {
    
    class InvalidSeedBytes(message: String) : BuildException(message)
    
    class InvalidSeedFile(message: String) : BuildException(message)
    
    class InvalidSystemTime(message: String) : BuildException(message)
    
    class InvalidChannelMonitor(message: String) : BuildException(message)
    
    class InvalidListeningAddresses(message: String) : BuildException(message)
    
    class InvalidAnnouncementAddresses(message: String) : BuildException(message)
    
    class InvalidNodeAlias(message: String) : BuildException(message)
    
    class RuntimeSetupFailed(message: String) : BuildException(message)
    
    class ReadFailed(message: String) : BuildException(message)
    
    class WriteFailed(message: String) : BuildException(message)
    
    class StoragePathAccessFailed(message: String) : BuildException(message)
    
    class KvStoreSetupFailed(message: String) : BuildException(message)
    
    class WalletSetupFailed(message: String) : BuildException(message)
    
    class LoggerSetupFailed(message: String) : BuildException(message)
    
    class NetworkMismatch(message: String) : BuildException(message)
    
    class AsyncPaymentsConfigMismatch(message: String) : BuildException(message)
    
}




@kotlinx.serialization.Serializable
sealed class ClosureReason {
    @kotlinx.serialization.Serializable
    data class CounterpartyForceClosed(
        val `peerMsg`: UntrustedString,
    ) : ClosureReason() {
    }
    @kotlinx.serialization.Serializable
    data class HolderForceClosed(
        val `broadcastedLatestTxn`: kotlin.Boolean?,
        val `message`: kotlin.String,
    ) : ClosureReason() {
    }
    
    @kotlinx.serialization.Serializable
    data object LegacyCooperativeClosure : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    data object CounterpartyInitiatedCooperativeClosure : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    data object LocallyInitiatedCooperativeClosure : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    data object CommitmentTxConfirmed : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    data object FundingTimedOut : ClosureReason() 
    
    @kotlinx.serialization.Serializable
    data class ProcessingError(
        val `err`: kotlin.String,
    ) : ClosureReason() {
    }
    
    @kotlinx.serialization.Serializable
    data object DisconnectedPeer : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    data object OutdatedChannelManager : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    data object CounterpartyCoopClosedUnfundedChannel : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    data object LocallyCoopClosedUnfundedChannel : ClosureReason() 
    
    
    @kotlinx.serialization.Serializable
    data object FundingBatchClosure : ClosureReason() 
    
    @kotlinx.serialization.Serializable
    data class HtlCsTimedOut(
        val `paymentHash`: PaymentHash?,
    ) : ClosureReason() {
    }
    @kotlinx.serialization.Serializable
    data class PeerFeerateTooLow(
        val `peerFeerateSatPerKw`: kotlin.UInt,
        val `requiredFeerateSatPerKw`: kotlin.UInt,
    ) : ClosureReason() {
    }
    
}







@kotlinx.serialization.Serializable
enum class CoinSelectionAlgorithm {
    
    BRANCH_AND_BOUND,
    LARGEST_FIRST,
    OLDEST_FIRST,
    SINGLE_RANDOM_DRAW;
    companion object
}






@kotlinx.serialization.Serializable
sealed class ConfirmationStatus {
    @kotlinx.serialization.Serializable
    data class Confirmed(
        val `blockHash`: BlockHash,
        val `height`: kotlin.UInt,
        val `timestamp`: kotlin.ULong,
    ) : ConfirmationStatus() {
    }
    
    @kotlinx.serialization.Serializable
    data object Unconfirmed : ConfirmationStatus() 
    
    
}







@kotlinx.serialization.Serializable
enum class Currency {
    
    BITCOIN,
    BITCOIN_TESTNET,
    REGTEST,
    SIMNET,
    SIGNET;
    companion object
}






@kotlinx.serialization.Serializable
sealed class Event {
    @kotlinx.serialization.Serializable
    data class PaymentSuccessful(
        val `paymentId`: PaymentId?,
        val `paymentHash`: PaymentHash,
        val `paymentPreimage`: PaymentPreimage?,
        val `feePaidMsat`: kotlin.ULong?,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class PaymentFailed(
        val `paymentId`: PaymentId?,
        val `paymentHash`: PaymentHash?,
        val `reason`: PaymentFailureReason?,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class PaymentReceived(
        val `paymentId`: PaymentId?,
        val `paymentHash`: PaymentHash,
        val `amountMsat`: kotlin.ULong,
        val `customRecords`: List<CustomTlvRecord>,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class PaymentClaimable(
        val `paymentId`: PaymentId,
        val `paymentHash`: PaymentHash,
        val `claimableAmountMsat`: kotlin.ULong,
        val `claimDeadline`: kotlin.UInt?,
        val `customRecords`: List<CustomTlvRecord>,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class PaymentForwarded(
        val `prevChannelId`: ChannelId,
        val `nextChannelId`: ChannelId,
        val `prevUserChannelId`: UserChannelId?,
        val `nextUserChannelId`: UserChannelId?,
        val `prevNodeId`: PublicKey?,
        val `nextNodeId`: PublicKey?,
        val `totalFeeEarnedMsat`: kotlin.ULong?,
        val `skimmedFeeMsat`: kotlin.ULong?,
        val `claimFromOnchainTx`: kotlin.Boolean,
        val `outboundAmountForwardedMsat`: kotlin.ULong?,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class ChannelPending(
        val `channelId`: ChannelId,
        val `userChannelId`: UserChannelId,
        val `formerTemporaryChannelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `fundingTxo`: OutPoint,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class ChannelReady(
        val `channelId`: ChannelId,
        val `userChannelId`: UserChannelId,
        val `counterpartyNodeId`: PublicKey?,
        val `fundingTxo`: OutPoint?,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class ChannelClosed(
        val `channelId`: ChannelId,
        val `userChannelId`: UserChannelId,
        val `counterpartyNodeId`: PublicKey?,
        val `reason`: ClosureReason?,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class SplicePending(
        val `channelId`: ChannelId,
        val `userChannelId`: UserChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `newFundingTxo`: OutPoint,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class SpliceFailed(
        val `channelId`: ChannelId,
        val `userChannelId`: UserChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `abandonedFundingTxo`: OutPoint?,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class OnchainTransactionConfirmed(
        val `txid`: Txid,
        val `blockHash`: BlockHash,
        val `blockHeight`: kotlin.UInt,
        val `confirmationTime`: kotlin.ULong,
        val `details`: TransactionDetails,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class OnchainTransactionReceived(
        val `txid`: Txid,
        val `details`: TransactionDetails,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class OnchainTransactionReplaced(
        val `txid`: Txid,
        val `conflicts`: List<Txid>,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class OnchainTransactionReorged(
        val `txid`: Txid,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class OnchainTransactionEvicted(
        val `txid`: Txid,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class SyncProgress(
        val `syncType`: SyncType,
        val `progressPercent`: kotlin.UByte,
        val `currentBlockHeight`: kotlin.UInt,
        val `targetBlockHeight`: kotlin.UInt,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class SyncCompleted(
        val `syncType`: SyncType,
        val `syncedBlockHeight`: kotlin.UInt,
    ) : Event() {
    }
    @kotlinx.serialization.Serializable
    data class BalanceChanged(
        val `oldSpendableOnchainBalanceSats`: kotlin.ULong,
        val `newSpendableOnchainBalanceSats`: kotlin.ULong,
        val `oldTotalOnchainBalanceSats`: kotlin.ULong,
        val `newTotalOnchainBalanceSats`: kotlin.ULong,
        val `oldTotalLightningBalanceSats`: kotlin.ULong,
        val `newTotalLightningBalanceSats`: kotlin.ULong,
    ) : Event() {
    }
    
}







@kotlinx.serialization.Serializable
enum class Lsps1PaymentState {
    
    EXPECT_PAYMENT,
    PAID,
    REFUNDED;
    companion object
}






@kotlinx.serialization.Serializable
sealed class LightningBalance {
    @kotlinx.serialization.Serializable
    data class ClaimableOnChannelClose(
        val `channelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `amountSatoshis`: kotlin.ULong,
        val `transactionFeeSatoshis`: kotlin.ULong,
        val `outboundPaymentHtlcRoundedMsat`: kotlin.ULong,
        val `outboundForwardedHtlcRoundedMsat`: kotlin.ULong,
        val `inboundClaimingHtlcRoundedMsat`: kotlin.ULong,
        val `inboundHtlcRoundedMsat`: kotlin.ULong,
    ) : LightningBalance() {
    }
    @kotlinx.serialization.Serializable
    data class ClaimableAwaitingConfirmations(
        val `channelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `amountSatoshis`: kotlin.ULong,
        val `confirmationHeight`: kotlin.UInt,
        val `source`: BalanceSource,
    ) : LightningBalance() {
    }
    @kotlinx.serialization.Serializable
    data class ContentiousClaimable(
        val `channelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `amountSatoshis`: kotlin.ULong,
        val `timeoutHeight`: kotlin.UInt,
        val `paymentHash`: PaymentHash,
        val `paymentPreimage`: PaymentPreimage,
    ) : LightningBalance() {
    }
    @kotlinx.serialization.Serializable
    data class MaybeTimeoutClaimableHtlc(
        val `channelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `amountSatoshis`: kotlin.ULong,
        val `claimableHeight`: kotlin.UInt,
        val `paymentHash`: PaymentHash,
        val `outboundPayment`: kotlin.Boolean,
    ) : LightningBalance() {
    }
    @kotlinx.serialization.Serializable
    data class MaybePreimageClaimableHtlc(
        val `channelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `amountSatoshis`: kotlin.ULong,
        val `expiryHeight`: kotlin.UInt,
        val `paymentHash`: PaymentHash,
    ) : LightningBalance() {
    }
    @kotlinx.serialization.Serializable
    data class CounterpartyRevokedOutputClaimable(
        val `channelId`: ChannelId,
        val `counterpartyNodeId`: PublicKey,
        val `amountSatoshis`: kotlin.ULong,
    ) : LightningBalance() {
    }
    
}







@kotlinx.serialization.Serializable
enum class LogLevel {
    
    GOSSIP,
    TRACE,
    DEBUG,
    INFO,
    WARN,
    ERROR;
    companion object
}






@kotlinx.serialization.Serializable
sealed class MaxDustHtlcExposure {
    @kotlinx.serialization.Serializable
    data class FixedLimit(
        val `limitMsat`: kotlin.ULong,
    ) : MaxDustHtlcExposure() {
    }
    @kotlinx.serialization.Serializable
    data class FeeRateMultiplier(
        val `multiplier`: kotlin.ULong,
    ) : MaxDustHtlcExposure() {
    }
    
}







@kotlinx.serialization.Serializable
enum class Network {
    
    BITCOIN,
    TESTNET,
    SIGNET,
    REGTEST;
    companion object
}







sealed class NodeException(message: String): kotlin.Exception(message) {
    
    class AlreadyRunning(message: String) : NodeException(message)
    
    class NotRunning(message: String) : NodeException(message)
    
    class OnchainTxCreationFailed(message: String) : NodeException(message)
    
    class ConnectionFailed(message: String) : NodeException(message)
    
    class InvoiceCreationFailed(message: String) : NodeException(message)
    
    class InvoiceRequestCreationFailed(message: String) : NodeException(message)
    
    class OfferCreationFailed(message: String) : NodeException(message)
    
    class RefundCreationFailed(message: String) : NodeException(message)
    
    class PaymentSendingFailed(message: String) : NodeException(message)
    
    class InvalidCustomTlvs(message: String) : NodeException(message)
    
    class ProbeSendingFailed(message: String) : NodeException(message)
    
    class RouteNotFound(message: String) : NodeException(message)
    
    class ChannelCreationFailed(message: String) : NodeException(message)
    
    class ChannelClosingFailed(message: String) : NodeException(message)
    
    class ChannelSplicingFailed(message: String) : NodeException(message)
    
    class ChannelConfigUpdateFailed(message: String) : NodeException(message)
    
    class PersistenceFailed(message: String) : NodeException(message)
    
    class FeerateEstimationUpdateFailed(message: String) : NodeException(message)
    
    class FeerateEstimationUpdateTimeout(message: String) : NodeException(message)
    
    class WalletOperationFailed(message: String) : NodeException(message)
    
    class WalletOperationTimeout(message: String) : NodeException(message)
    
    class OnchainTxSigningFailed(message: String) : NodeException(message)
    
    class TxSyncFailed(message: String) : NodeException(message)
    
    class TxSyncTimeout(message: String) : NodeException(message)
    
    class GossipUpdateFailed(message: String) : NodeException(message)
    
    class GossipUpdateTimeout(message: String) : NodeException(message)
    
    class LiquidityRequestFailed(message: String) : NodeException(message)
    
    class UriParameterParsingFailed(message: String) : NodeException(message)
    
    class InvalidAddress(message: String) : NodeException(message)
    
    class InvalidSocketAddress(message: String) : NodeException(message)
    
    class InvalidPublicKey(message: String) : NodeException(message)
    
    class InvalidSecretKey(message: String) : NodeException(message)
    
    class InvalidOfferId(message: String) : NodeException(message)
    
    class InvalidNodeId(message: String) : NodeException(message)
    
    class InvalidPaymentId(message: String) : NodeException(message)
    
    class InvalidPaymentHash(message: String) : NodeException(message)
    
    class InvalidPaymentPreimage(message: String) : NodeException(message)
    
    class InvalidPaymentSecret(message: String) : NodeException(message)
    
    class InvalidAmount(message: String) : NodeException(message)
    
    class InvalidInvoice(message: String) : NodeException(message)
    
    class InvalidOffer(message: String) : NodeException(message)
    
    class InvalidRefund(message: String) : NodeException(message)
    
    class InvalidChannelId(message: String) : NodeException(message)
    
    class InvalidNetwork(message: String) : NodeException(message)
    
    class InvalidUri(message: String) : NodeException(message)
    
    class InvalidQuantity(message: String) : NodeException(message)
    
    class InvalidNodeAlias(message: String) : NodeException(message)
    
    class InvalidDateTime(message: String) : NodeException(message)
    
    class InvalidFeeRate(message: String) : NodeException(message)
    
    class DuplicatePayment(message: String) : NodeException(message)
    
    class UnsupportedCurrency(message: String) : NodeException(message)
    
    class InsufficientFunds(message: String) : NodeException(message)
    
    class LiquiditySourceUnavailable(message: String) : NodeException(message)
    
    class LiquidityFeeTooHigh(message: String) : NodeException(message)
    
    class InvalidBlindedPaths(message: String) : NodeException(message)
    
    class AsyncPaymentServicesDisabled(message: String) : NodeException(message)
    
    class CannotRbfFundingTransaction(message: String) : NodeException(message)
    
    class TransactionNotFound(message: String) : NodeException(message)
    
    class TransactionAlreadyConfirmed(message: String) : NodeException(message)
    
    class NoSpendableOutputs(message: String) : NodeException(message)
    
    class CoinSelectionFailed(message: String) : NodeException(message)
    
    class InvalidMnemonic(message: String) : NodeException(message)
    
    class BackgroundSyncNotEnabled(message: String) : NodeException(message)
    
}




@kotlinx.serialization.Serializable
sealed class OfferAmount {
    @kotlinx.serialization.Serializable
    data class Bitcoin(
        val `amountMsats`: kotlin.ULong,
    ) : OfferAmount() {
    }
    @kotlinx.serialization.Serializable
    data class Currency(
        val `iso4217Code`: kotlin.String,
        val `amount`: kotlin.ULong,
    ) : OfferAmount() {
    }
    
}







@kotlinx.serialization.Serializable
enum class PaymentDirection {
    
    INBOUND,
    OUTBOUND;
    companion object
}







@kotlinx.serialization.Serializable
enum class PaymentFailureReason {
    
    RECIPIENT_REJECTED,
    USER_ABANDONED,
    RETRIES_EXHAUSTED,
    PAYMENT_EXPIRED,
    ROUTE_NOT_FOUND,
    UNEXPECTED_ERROR,
    UNKNOWN_REQUIRED_FEATURES,
    INVOICE_REQUEST_EXPIRED,
    INVOICE_REQUEST_REJECTED,
    BLINDED_PATH_CREATION_FAILED;
    companion object
}






@kotlinx.serialization.Serializable
sealed class PaymentKind {
    @kotlinx.serialization.Serializable
    data class Onchain(
        val `txid`: Txid,
        val `status`: ConfirmationStatus,
    ) : PaymentKind() {
    }
    @kotlinx.serialization.Serializable
    data class Bolt11(
        val `hash`: PaymentHash,
        val `preimage`: PaymentPreimage?,
        val `secret`: PaymentSecret?,
        val `description`: kotlin.String?,
        val `bolt11`: kotlin.String?,
    ) : PaymentKind() {
    }
    @kotlinx.serialization.Serializable
    data class Bolt11Jit(
        val `hash`: PaymentHash,
        val `preimage`: PaymentPreimage?,
        val `secret`: PaymentSecret?,
        val `counterpartySkimmedFeeMsat`: kotlin.ULong?,
        val `lspFeeLimits`: LspFeeLimits,
        val `description`: kotlin.String?,
        val `bolt11`: kotlin.String?,
    ) : PaymentKind() {
    }
    @kotlinx.serialization.Serializable
    data class Bolt12Offer(
        val `hash`: PaymentHash?,
        val `preimage`: PaymentPreimage?,
        val `secret`: PaymentSecret?,
        val `offerId`: OfferId,
        val `payerNote`: UntrustedString?,
        val `quantity`: kotlin.ULong?,
    ) : PaymentKind() {
    }
    @kotlinx.serialization.Serializable
    data class Bolt12Refund(
        val `hash`: PaymentHash?,
        val `preimage`: PaymentPreimage?,
        val `secret`: PaymentSecret?,
        val `payerNote`: UntrustedString?,
        val `quantity`: kotlin.ULong?,
    ) : PaymentKind() {
    }
    @kotlinx.serialization.Serializable
    data class Spontaneous(
        val `hash`: PaymentHash,
        val `preimage`: PaymentPreimage?,
    ) : PaymentKind() {
    }
    
}







@kotlinx.serialization.Serializable
enum class PaymentStatus {
    
    PENDING,
    SUCCEEDED,
    FAILED;
    companion object
}






@kotlinx.serialization.Serializable
sealed class PendingSweepBalance {
    @kotlinx.serialization.Serializable
    data class PendingBroadcast(
        val `channelId`: ChannelId?,
        val `amountSatoshis`: kotlin.ULong,
    ) : PendingSweepBalance() {
    }
    @kotlinx.serialization.Serializable
    data class BroadcastAwaitingConfirmation(
        val `channelId`: ChannelId?,
        val `latestBroadcastHeight`: kotlin.UInt,
        val `latestSpendingTxid`: Txid,
        val `amountSatoshis`: kotlin.ULong,
    ) : PendingSweepBalance() {
    }
    @kotlinx.serialization.Serializable
    data class AwaitingThresholdConfirmations(
        val `channelId`: ChannelId?,
        val `latestSpendingTxid`: Txid,
        val `confirmationHash`: BlockHash,
        val `confirmationHeight`: kotlin.UInt,
        val `amountSatoshis`: kotlin.ULong,
    ) : PendingSweepBalance() {
    }
    
}






@kotlinx.serialization.Serializable
sealed class QrPaymentResult {
    @kotlinx.serialization.Serializable
    data class Onchain(
        val `txid`: Txid,
    ) : QrPaymentResult() {
    }
    @kotlinx.serialization.Serializable
    data class Bolt11(
        val `paymentId`: PaymentId,
    ) : QrPaymentResult() {
    }
    @kotlinx.serialization.Serializable
    data class Bolt12(
        val `paymentId`: PaymentId,
    ) : QrPaymentResult() {
    }
    
}







@kotlinx.serialization.Serializable
enum class SyncType {
    
    ONCHAIN_WALLET,
    LIGHTNING_WALLET,
    FEE_RATE_CACHE;
    companion object
}







sealed class VssHeaderProviderException(message: String): kotlin.Exception(message) {
    
    class InvalidData(message: String) : VssHeaderProviderException(message)
    
    class RequestException(message: String) : VssHeaderProviderException(message)
    
    class AuthorizationException(message: String) : VssHeaderProviderException(message)
    
    class InternalException(message: String) : VssHeaderProviderException(message)
    
}





@kotlinx.serialization.Serializable
enum class WordCount {
    
    WORDS12,
    WORDS15,
    WORDS18,
    WORDS21,
    WORDS24;
    companion object
}









































































































































/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias Address = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias BlockHash = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias ChannelId = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias Lsps1OrderId = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias LspsDateTime = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias Mnemonic = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias NodeAlias = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias NodeId = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias OfferId = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias PaymentHash = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias PaymentId = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias PaymentPreimage = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias PaymentSecret = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias PublicKey = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias SocketAddress = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias Txid = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias UntrustedString = kotlin.String



/**
 * Typealias from the type name used in the UDL file to the builtin type.  This
 * is needed because the UDL type name is used in function/method signatures.
 * It's also what we have an external type that references a custom type.
 */
typealias UserChannelId = kotlin.String

