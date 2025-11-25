# LDK Node - New Events API Guide for Mobile Developers

## Overview

This guide covers the new event system added to LDK Node that provides real-time notifications for wallet and Lightning channel state changes. These events eliminate the need for polling and provide instant updates for better user experience and battery efficiency.

## Table of Contents
1. [New Event Types](#new-event-types)
2. [iOS/Swift Implementation](#iosswift-implementation)
3. [Android/Kotlin Implementation](#androidkotlin-implementation)
4. [Migration Guide](#migration-guide)
5. [Best Practices](#best-practices)
6. [Testing Recommendations](#testing-recommendations)

---

## New Event Types

### 1. Onchain Transaction Events

These events notify you about Bitcoin transactions affecting your onchain wallet:

#### `OnchainTransactionReceived`
- **When**: New unconfirmed transaction detected in mempool
- **Use Case**: Show "Payment incoming!" notification immediately
- **Fields**:
  - `txid`: Transaction ID
  - `details`: `TransactionDetails` object with comprehensive transaction information

#### `OnchainTransactionConfirmed`
- **When**: Transaction receives blockchain confirmations
- **Use Case**: Update transaction status from pending to confirmed
- **Fields**:
  - `txid`: Transaction ID
  - `blockHash`: Block hash where confirmed
  - `blockHeight`: Block height
  - `confirmationTime`: Unix timestamp
  - `details`: `TransactionDetails` object with comprehensive transaction information

#### `OnchainTransactionReplaced`
- **When**: Transaction is replaced via Replace-By-Fee (RBF)
- **Use Case**: Update UI to show the transaction was replaced
- **Fields**:
  - `txid`: Transaction ID of the transaction that was replaced (the old transaction)
  - `conflicts`: Array of transaction IDs that replaced this transaction (the replacement transactions)

#### `OnchainTransactionReorged`
- **When**: Previously confirmed transaction becomes unconfirmed due to blockchain reorg
- **Use Case**: Mark transaction as pending again
- **Fields**:
  - `txid`: Transaction ID

#### `OnchainTransactionEvicted`
- **When**: Transaction is evicted from the mempool (no longer unconfirmed and not confirmed)
- **Use Case**: Mark transaction as evicted, potentially allow user to rebroadcast
- **Fields**:
  - `txid`: Transaction ID
- **Note**: Works with all chain sources (Esplora, Electrum, and BitcoindRpc)

### 2. Sync Events

Track synchronization progress and completion:

#### `SyncProgress`
- **When**: Periodically during sync operations
- **Use Case**: Show progress bar during sync
- **Fields**:
  - `syncType`: What's syncing (OnchainWallet, LightningWallet, FeeRateCache)
  - `progressPercent`: 0-100
  - `currentBlockHeight`: Current sync position
  - `targetBlockHeight`: Target height

#### `SyncCompleted`
- **When**: Sync operation finishes successfully
- **Use Case**: Hide progress indicators, enable UI
- **Fields**:
  - `syncType`: What completed syncing
  - `syncedBlockHeight`: Final synced height

### 3. Balance Events

#### `BalanceChanged`
- **When**: Onchain or Lightning balance changes
- **Use Case**: Update balance display immediately
- **Fields**:
  - `oldSpendableOnchainBalanceSats`: Previous spendable onchain balance
  - `newSpendableOnchainBalanceSats`: New spendable onchain balance
  - `oldTotalOnchainBalanceSats`: Previous total onchain (including unconfirmed)
  - `newTotalOnchainBalanceSats`: New total onchain
  - `oldTotalLightningBalanceSats`: Previous Lightning balance
  - `newTotalLightningBalanceSats`: New Lightning balance

### 4. Transaction Details

The `TransactionDetails` struct provides comprehensive information about transactions, allowing you to analyze transaction purposes yourself:

#### `TransactionDetails`
- `amountSats`: Net amount in satoshis (positive for incoming, negative for outgoing)
- `inputs`: Array of `TxInput` objects
- `outputs`: Array of `TxOutput` objects

#### `TxInput`
- `txid`: Previous transaction ID being spent
- `vout`: Output index being spent
- `scriptsig`: Script signature (hex-encoded)
- `witness`: Witness stack (array of hex-encoded strings)
- `sequence`: Sequence number

#### `TxOutput`
- `scriptpubkey`: Script public key (hex-encoded)
- `scriptpubkeyType`: Script type (e.g., "p2wpkh", "p2wsh", "p2tr")
- `scriptpubkeyAddress`: Bitcoin address (if decodable)
- `value`: Value in satoshis
- `n`: Output index

**Note**: You can analyze transaction inputs and outputs to detect channel funding, channel closures, and other transaction types. This provides more flexibility than the previous `TransactionContext` enum.

#### Retrieving Transaction Details

You can also retrieve transaction details directly using `Node::get_transaction_details()`:

```swift
// Get transaction details for a specific transaction ID
if let details = node.getTransactionDetails(txid: txid) {
    // Analyze the transaction details
    print("Transaction amount: \(details.amountSats) sats")
    print("Number of inputs: \(details.inputs.count)")
    print("Number of outputs: \(details.outputs.count)")
} else {
    // Transaction not found in wallet
    print("Transaction not found")
}
```

This method returns `nil` if the transaction is not found in the wallet.

#### Retrieving Address Balance

You can retrieve the current balance for any Bitcoin address using `Node::get_address_balance()`:

```swift
// Get balance for a Bitcoin address
do {
    let balance = try node.getAddressBalance(addressStr: "bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh")
    print("Address balance: \(balance) sats")
} catch {
    // Invalid address or network mismatch
    print("Error: \(error)")
}
```

```kotlin
// Get balance for a Bitcoin address
try {
    val balance = node.getAddressBalance("bc1qxy2kgdygjrsqtzq2n0yrf2493p83kkfjhx0wlh")
    println("Address balance: $balance sats")
} catch (e: Exception) {
    // Invalid address or network mismatch
    println("Error: ${e.message}")
}
```

**Note**: This method queries the chain source directly and returns the balance in satoshis. It throws an error if the address string cannot be parsed or doesn't match the node's network. It returns `0` if the balance cannot be queried (e.g., chain source unavailable). This method is not available when using BitcoindRpc as the chain source.

---

## iOS/Swift Implementation

### Setup and Basic Event Handling

```swift
import LDKNode

class WalletEventHandler {
    private let node: Node
    private var eventTimer: Timer?

    init(node: Node) {
        self.node = node
        startEventHandling()
    }

    func startEventHandling() {
        // Check for events every 100ms
        eventTimer = Timer.scheduledTimer(withTimeInterval: 0.1, repeats: true) { _ in
            self.processNextEvent()
        }
    }

    func processNextEvent() {
        guard let event = node.nextEvent() else { return }

        handleEvent(event)
        node.eventHandled()
    }

    func handleEvent(_ event: Event) {
        switch event {
        case .onchainTransactionReceived(let txid, let details):
            handleIncomingTransaction(txid: txid, details: details)

        case .onchainTransactionConfirmed(let txid, let blockHash, let blockHeight, let confirmationTime, let details):
            handleConfirmedTransaction(txid: txid, height: blockHeight, details: details)

        case .onchainTransactionReplaced(let txid, let conflicts):
            handleReplacedTransaction(txid: txid, conflicts: conflicts)

        case .onchainTransactionReorged(let txid):
            handleReorgedTransaction(txid: txid)

        case .onchainTransactionEvicted(let txid):
            handleEvictedTransaction(txid: txid)

        case .syncProgress(let syncType, let progressPercent, let currentBlock, let targetBlock):
            updateSyncProgress(type: syncType, percent: progressPercent)

        case .syncCompleted(let syncType, let syncedHeight):
            handleSyncCompleted(type: syncType, height: syncedHeight)

        case .balanceChanged(let oldSpendable, let newSpendable, let oldTotal, let newTotal, let oldLightning, let newLightning):
            updateBalances(onchain: newSpendable, lightning: newLightning)

        default:
            // Handle other existing events
            break
        }
    }
}
```

### Practical Examples

#### Example 1: Real-time Transaction Notifications

```swift
func handleIncomingTransaction(txid: String, details: TransactionDetails) {
    DispatchQueue.main.async {
        if details.amountSats > 0 {
            // Incoming payment
            self.showNotification(
                title: "Payment Received!",
                body: "Incoming payment of \(details.amountSats) sats (unconfirmed)",
                txid: txid
            )

            // Update UI to show pending transaction
            self.addPendingTransaction(txid: txid, amount: details.amountSats)

            // Play sound or haptic feedback
            self.playPaymentReceivedSound()
        } else {
            // Outgoing payment
            self.updateTransactionStatus(txid: txid, status: .broadcasting)
        }

        // Analyze transaction to detect channel-related transactions
        if self.isChannelFundingTransaction(details: details) {
            print("Channel funding transaction detected")
        } else if self.isChannelClosureTransaction(details: details) {
            print("Channel closure transaction detected")
        }
    }
}

func handleConfirmedTransaction(txid: String, height: UInt32, details: TransactionDetails) {
    DispatchQueue.main.async {
        // Update transaction status
        self.updateTransactionStatus(txid: txid, status: .confirmed(height: height))

        // Show confirmation notification
        self.showNotification(
            title: "Payment Confirmed!",
            body: "Transaction confirmed at block \(height)",
            txid: txid
        )

        // Refresh transaction list
        self.refreshTransactionList()
    }
}

func handleReplacedTransaction(txid: String, conflicts: [String]) {
    DispatchQueue.main.async {
        self.updateTransactionStatus(txid: txid, status: .replaced)
        
        if conflicts.isEmpty {
            self.showNotification(
                title: "Transaction Replaced",
                body: "Transaction was replaced via RBF",
                txid: txid
            )
        } else {
            self.showNotification(
                title: "Transaction Replaced",
                body: "Transaction was replaced by \(conflicts.count) transaction(s)",
                txid: txid
            )
            // Track replacement transactions
            for conflictTxid in conflicts {
                self.trackReplacementTransaction(originalTxid: txid, replacementTxid: conflictTxid)
            }
        }
    }
}

func handleReorgedTransaction(txid: String) {
    DispatchQueue.main.async {
        self.updateTransactionStatus(txid: txid, status: .pending)
        self.showNotification(
            title: "Transaction Unconfirmed",
            body: "Transaction became unconfirmed due to reorg",
            txid: txid
        )
    }
}

func handleEvictedTransaction(txid: String) {
    DispatchQueue.main.async {
        self.updateTransactionStatus(txid: txid, status: .evicted)
        self.showNotification(
            title: "Transaction Evicted",
            body: "Transaction was evicted from mempool",
            txid: txid
        )
        // Optionally allow user to rebroadcast
        self.promptRebroadcast(txid: txid)
    }
}

// Helper functions to analyze transaction details
func isChannelFundingTransaction(details: TransactionDetails) -> Bool {
    // Analyze inputs/outputs to detect channel funding
    // This is a simplified example - implement based on your needs
    return details.outputs.count == 2 && details.amountSats > 0
}

func isChannelClosureTransaction(details: TransactionDetails) -> Bool {
    // Analyze inputs/outputs to detect channel closure
    // This is a simplified example - implement based on your needs
    return details.inputs.count > 0 && details.outputs.count >= 2
}
```

#### Example 2: Sync Progress Bar

```swift
class SyncProgressView: UIView {
    @IBOutlet weak var progressBar: UIProgressView!
    @IBOutlet weak var statusLabel: UILabel!
    @IBOutlet weak var blockHeightLabel: UILabel!

    func updateSyncProgress(type: SyncType, percent: UInt8) {
        DispatchQueue.main.async {
            self.progressBar.progress = Float(percent) / 100.0

            switch type {
            case .onchainWallet:
                self.statusLabel.text = "Syncing wallet: \(percent)%"
            case .lightningWallet:
                self.statusLabel.text = "Syncing Lightning: \(percent)%"
            case .feeRateCache:
                self.statusLabel.text = "Updating fee rates: \(percent)%"
            }

            self.progressBar.isHidden = false
        }
    }

    func handleSyncCompleted(type: SyncType, height: UInt32) {
        DispatchQueue.main.async {
            self.progressBar.isHidden = true
            self.statusLabel.text = "Synced to block \(height)"
            self.blockHeightLabel.text = "\(height)"

            // Enable send/receive buttons
            self.enableTransactionButtons()
        }
    }
}
```

#### Example 3: Live Balance Updates

```swift
class BalanceViewController: UIViewController {
    @IBOutlet weak var onchainBalanceLabel: UILabel!
    @IBOutlet weak var lightningBalanceLabel: UILabel!
    @IBOutlet weak var totalBalanceLabel: UILabel!

    func updateBalances(onchain: UInt64, lightning: UInt64) {
        DispatchQueue.main.async {
            // Format with thousand separators
            let formatter = NumberFormatter()
            formatter.numberStyle = .decimal
            formatter.groupingSeparator = ","

            let onchainFormatted = formatter.string(from: NSNumber(value: onchain)) ?? "0"
            let lightningFormatted = formatter.string(from: NSNumber(value: lightning)) ?? "0"
            let totalFormatted = formatter.string(from: NSNumber(value: onchain + lightning)) ?? "0"

            // Animate the balance change
            UIView.animate(withDuration: 0.3) {
                self.onchainBalanceLabel.alpha = 0.5
                self.lightningBalanceLabel.alpha = 0.5
            } completion: { _ in
                self.onchainBalanceLabel.text = "\(onchainFormatted) sats"
                self.lightningBalanceLabel.text = "\(lightningFormatted) sats"
                self.totalBalanceLabel.text = "\(totalFormatted) sats"

                UIView.animate(withDuration: 0.3) {
                    self.onchainBalanceLabel.alpha = 1.0
                    self.lightningBalanceLabel.alpha = 1.0
                }
            }

            // Flash green for increase, red for decrease
            if onchain > self.previousOnchainBalance {
                self.flashColor(self.onchainBalanceLabel, color: .systemGreen)
            }
        }
    }

    private func flashColor(_ label: UILabel, color: UIColor) {
        let originalColor = label.textColor
        label.textColor = color
        UIView.animate(withDuration: 1.0) {
            label.textColor = originalColor
        }
    }
}
```

---

## Android/Kotlin Implementation

### Setup and Basic Event Handling

```kotlin
import org.lightningdevkit.ldknode.*
import kotlinx.coroutines.*

class WalletEventHandler(private val node: Node) {
    private var eventJob: Job? = null

    fun startEventHandling() {
        eventJob = GlobalScope.launch {
            while (isActive) {
                processNextEvent()
                delay(100) // Check every 100ms
            }
        }
    }

    fun stopEventHandling() {
        eventJob?.cancel()
    }

    private fun processNextEvent() {
        val event = node.nextEvent() ?: return

        handleEvent(event)
        node.eventHandled()
    }

    private fun handleEvent(event: Event) {
        when (event) {
            is Event.OnchainTransactionReceived -> {
                handleIncomingTransaction(event.txid, event.details)
            }

            is Event.OnchainTransactionConfirmed -> {
                handleConfirmedTransaction(event.txid, event.blockHeight, event.details)
            }

            is Event.OnchainTransactionReplaced -> {
                handleReplacedTransaction(event.txid, event.conflicts)
            }

            is Event.OnchainTransactionReorged -> {
                handleReorgedTransaction(event.txid)
            }

            is Event.OnchainTransactionEvicted -> {
                handleEvictedTransaction(event.txid)
            }

            is Event.SyncProgress -> {
                updateSyncProgress(
                    event.syncType,
                    event.progressPercent
                )
            }

            is Event.SyncCompleted -> {
                handleSyncCompleted(
                    event.syncType,
                    event.syncedBlockHeight
                )
            }

            is Event.BalanceChanged -> {
                updateBalances(
                    event.newSpendableOnchainBalanceSats,
                    event.newTotalLightningBalanceSats
                )
            }

            else -> {
                // Handle other existing events
            }
        }
    }
}
```

### Practical Examples

#### Example 1: Real-time Transaction Notifications

```kotlin
class TransactionNotificationManager(private val context: Context) {

    fun handleIncomingTransaction(txid: String, details: TransactionDetails) {
        GlobalScope.launch(Dispatchers.Main) {
            if (details.amountSats > 0) {
                // Incoming payment
                showNotification(
                    title = "Payment Received!",
                    message = "Incoming payment of ${details.amountSats} sats (unconfirmed)",
                    txid = txid
                )

                // Update transaction list
                addPendingTransaction(txid, details.amountSats)

                // Play sound
                playPaymentSound()
            } else {
                // Outgoing payment
                updateTransactionStatus(txid, TransactionStatus.BROADCASTING)
            }

            // Analyze transaction to detect channel-related transactions
            if (isChannelFundingTransaction(details)) {
                Log.d("Wallet", "Channel funding transaction detected")
            } else if (isChannelClosureTransaction(details)) {
                Log.d("Wallet", "Channel closure transaction detected")
            }
        }
    }

    fun handleConfirmedTransaction(txid: String, height: UInt, details: TransactionDetails) {
        GlobalScope.launch(Dispatchers.Main) {
            // Update status
            updateTransactionStatus(txid, TransactionStatus.CONFIRMED)

            // Show notification
            showNotification(
                title = "Payment Confirmed!",
                message = "Transaction confirmed at block $height",
                txid = txid
            )

            // Vibrate
            vibrate()

            // Refresh transaction list
            refreshTransactionList()
        }
    }

    fun handleReplacedTransaction(txid: String, conflicts: List<String>) {
        GlobalScope.launch(Dispatchers.Main) {
            updateTransactionStatus(txid, TransactionStatus.REPLACED)
            
            if (conflicts.isEmpty()) {
                showNotification(
                    title = "Transaction Replaced",
                    message = "Transaction was replaced via RBF",
                    txid = txid
                )
            } else {
                showNotification(
                    title = "Transaction Replaced",
                    message = "Transaction was replaced by ${conflicts.size} transaction(s)",
                    txid = txid
                )
                // Track replacement transactions
                conflicts.forEach { conflictTxid ->
                    trackReplacementTransaction(originalTxid = txid, replacementTxid = conflictTxid)
                }
            }
        }
    }

    fun handleReorgedTransaction(txid: String) {
        GlobalScope.launch(Dispatchers.Main) {
            updateTransactionStatus(txid, TransactionStatus.PENDING)
            showNotification(
                title = "Transaction Unconfirmed",
                message = "Transaction became unconfirmed due to reorg",
                txid = txid
            )
        }
    }

    fun handleEvictedTransaction(txid: String) {
        GlobalScope.launch(Dispatchers.Main) {
            updateTransactionStatus(txid, TransactionStatus.EVICTED)
            showNotification(
                title = "Transaction Evicted",
                message = "Transaction was evicted from mempool",
                txid = txid
            )
            // Optionally allow user to rebroadcast
            promptRebroadcast(txid)
        }
    }

    // Retrieve transaction details directly
    fun getTransactionDetails(txid: String): TransactionDetails? {
        return node.getTransactionDetails(txid = txid)
    }

    // Helper functions to analyze transaction details
    private fun isChannelFundingTransaction(details: TransactionDetails): Boolean {
        // Analyze inputs/outputs to detect channel funding
        // This is a simplified example - implement based on your needs
        return details.outputs.size == 2 && details.amountSats > 0
    }

    private fun isChannelClosureTransaction(details: TransactionDetails): Boolean {
        // Analyze inputs/outputs to detect channel closure
        // This is a simplified example - implement based on your needs
        return details.inputs.isNotEmpty() && details.outputs.size >= 2
    }
        }
    }

    private fun showNotification(title: String, message: String, txid: String) {
        val notificationManager = context.getSystemService(Context.NOTIFICATION_SERVICE) as NotificationManager

        val notification = NotificationCompat.Builder(context, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_bitcoin)
            .setContentTitle(title)
            .setContentText(message)
            .setPriority(NotificationCompat.PRIORITY_HIGH)
            .setAutoCancel(true)
            .build()

        notificationManager.notify(txid.hashCode(), notification)
    }
}
```

#### Example 2: Sync Progress Implementation

```kotlin
class SyncProgressFragment : Fragment() {
    private lateinit var binding: FragmentSyncProgressBinding

    fun updateSyncProgress(type: SyncType, percent: UByte) {
        activity?.runOnUiThread {
            binding.progressBar.progress = percent.toInt()
            binding.progressText.text = "$percent%"

            binding.statusText.text = when (type) {
                SyncType.ONCHAIN_WALLET -> "Syncing wallet..."
                SyncType.LIGHTNING_WALLET -> "Syncing Lightning..."
                SyncType.FEE_RATE_CACHE -> "Updating fee rates..."
            }

            binding.progressContainer.visibility = View.VISIBLE
        }
    }

    fun handleSyncCompleted(type: SyncType, height: UInt) {
        activity?.runOnUiThread {
            binding.progressContainer.visibility = View.GONE
            binding.statusText.text = "Synced to block $height"

            // Enable transaction buttons
            binding.sendButton.isEnabled = true
            binding.receiveButton.isEnabled = true

            // Show success message
            Snackbar.make(
                binding.root,
                "Sync completed!",
                Snackbar.LENGTH_SHORT
            ).show()
        }
    }
}
```

#### Example 3: Live Balance Updates with Animation

```kotlin
class BalanceViewModel : ViewModel() {
    private val _onchainBalance = MutableLiveData<ULong>()
    val onchainBalance: LiveData<ULong> = _onchainBalance

    private val _lightningBalance = MutableLiveData<ULong>()
    val lightningBalance: LiveData<ULong> = _lightningBalance

    fun updateBalances(onchain: ULong, lightning: ULong) {
        _onchainBalance.postValue(onchain)
        _lightningBalance.postValue(lightning)
    }
}

class BalanceFragment : Fragment() {
    private lateinit var binding: FragmentBalanceBinding
    private lateinit var viewModel: BalanceViewModel

    override fun onViewCreated(view: View, savedInstanceState: Bundle?) {
        super.onViewCreated(view, savedInstanceState)

        // Observe balance changes
        viewModel.onchainBalance.observe(viewLifecycleOwner) { balance ->
            animateBalanceUpdate(binding.onchainBalanceText, balance)
        }

        viewModel.lightningBalance.observe(viewLifecycleOwner) { balance ->
            animateBalanceUpdate(binding.lightningBalanceText, balance)
        }
    }

    private fun animateBalanceUpdate(textView: TextView, newBalance: ULong) {
        // Format with thousand separators
        val formatted = NumberFormat.getNumberInstance(Locale.US)
            .format(newBalance.toLong())

        // Animate the change
        ValueAnimator.ofFloat(0.5f, 1.0f).apply {
            duration = 300
            addUpdateListener { animation ->
                textView.alpha = animation.animatedValue as Float
            }
            start()
        }

        textView.text = "$formatted sats"

        // Flash color for visual feedback
        val originalColor = textView.currentTextColor
        textView.setTextColor(Color.GREEN)
        textView.postDelayed({
            textView.setTextColor(originalColor)
        }, 500)
    }
}
```

#### Example 4: Handling Chain Reorganizations

```kotlin
class ReorgHandler(private val database: TransactionDatabase) {

    fun handleUnconfirmedTransaction(txid: String) {
        GlobalScope.launch {
            // Mark transaction as unconfirmed in database
            database.updateTransactionStatus(txid, TransactionStatus.UNCONFIRMED)

            // Show warning to user
            withContext(Dispatchers.Main) {
                AlertDialog.Builder(context)
                    .setTitle("Transaction Unconfirmed")
                    .setMessage("Transaction $txid has become unconfirmed due to a blockchain reorganization. It may confirm again soon.")
                    .setPositiveButton("OK", null)
                    .show()
            }

            // Log the event
            Log.w("Wallet", "Transaction $txid unconfirmed due to reorg")
        }
    }
}
```

---

## Migration Guide

### Moving from Polling to Events

#### Before (Polling - Bad for Battery):
```kotlin
// DON'T DO THIS - Wastes battery and CPU
class OldWalletManager {
    fun startPolling() {
        timer.schedule(object : TimerTask() {
            override fun run() {
                // This runs even when nothing changed!
                val balances = node.listBalances()
                updateUI(balances)

                // Sync every time
                node.syncWallets()
            }
        }, 0, 30000) // Every 30 seconds
    }
}
```

#### After (Event-Driven - Efficient):
```kotlin
// DO THIS - Only runs when something actually happens
class NewWalletManager {
    fun startEventHandling() {
        GlobalScope.launch {
            while (isActive) {
                // This blocks until an event arrives - zero CPU usage while waiting!
                val event = node.waitNextEvent()

                when (event) {
                    is Event.BalanceChanged -> updateUI(event)
                    is Event.OnchainTransactionReceived -> notify(event)
                    // ... handle other events
                }

                node.eventHandled()
            }
        }
    }
}
```

### Key Differences:
1. **No more timers** - Events arrive when things actually happen
2. **No more manual sync calls** - Background sync is automatic with events
3. **Instant updates** - Users see changes immediately, not after next poll
4. **Better battery life** - CPU sleeps between events

---

## Best Practices

### 1. Always Handle Events After Processing
```kotlin
// ALWAYS call eventHandled() after processing
val event = node.nextEvent()
if (event != null) {
    processEvent(event)
    node.eventHandled() // Critical - marks event as processed
}
```

### 2. Use Background Threads for Event Loop
```swift
// iOS - Use background queue
DispatchQueue.global(qos: .background).async {
    while self.isRunning {
        if let event = self.node.nextEvent() {
            self.handleEvent(event)
            self.node.eventHandled()
        }
        Thread.sleep(forTimeInterval: 0.1)
    }
}
```

```kotlin
// Android - Use coroutines
GlobalScope.launch(Dispatchers.IO) {
    while (isActive) {
        node.nextEvent()?.let { event ->
            handleEvent(event)
            node.eventHandled()
        }
        delay(100)
    }
}
```

### 3. Update UI on Main Thread
```swift
// iOS
DispatchQueue.main.async {
    self.balanceLabel.text = "\(balance) sats"
}
```

```kotlin
// Android
runOnUiThread {
    balanceTextView.text = "$balance sats"
}
```

### 4. Handle All Event Types
Even if you don't use them all, handle gracefully:
```kotlin
when (event) {
    is Event.OnchainTransactionReceived -> handleTx(event)
    is Event.BalanceChanged -> updateBalance(event)
    // ... other events
    else -> Log.d("Events", "Unhandled event: $event")
}
```

### 5. Persist Important State
Events may arrive while app is in background:
```kotlin
fun handleBalanceChanged(event: Event.BalanceChanged) {
    // Save to persistent storage
    preferences.edit()
        .putLong("onchain_balance", event.newSpendableOnchainBalanceSats.toLong())
        .putLong("lightning_balance", event.newTotalLightningBalanceSats.toLong())
        .apply()

    // Then update UI if visible
    if (isResumed) {
        updateBalanceUI()
    }
}
```

---

## Testing Recommendations

### 1. Test Event Reception
```kotlin
@Test
fun testEventReception() {
    // Fund the wallet
    val address = node.onchainPayment().newAddress()
    sendTestCoins(address, 100000)

    // Sync to detect transaction
    node.syncWallets()

    // Should receive events
    var receivedEvent = false
    for (i in 0..10) {
        val event = node.nextEvent()
        if (event is Event.OnchainTransactionReceived) {
            receivedEvent = true
            assertEquals(100000, event.amountSats)
            node.eventHandled()
            break
        }
        Thread.sleep(100)
    }

    assertTrue("Should receive transaction event", receivedEvent)
}
```

### 2. Test Balance Change Events
```kotlin
@Test
fun testBalanceChangeEvent() {
    val initialBalance = node.listBalances().totalOnchainBalanceSats

    // Trigger a balance change
    fundWallet(50000)
    node.syncWallets()

    // Check for balance change event
    var balanceChanged = false
    for (i in 0..10) {
        val event = node.nextEvent()
        if (event is Event.BalanceChanged) {
            assertEquals(initialBalance, event.oldTotalOnchainBalanceSats)
            assertEquals(initialBalance + 50000, event.newTotalOnchainBalanceSats)
            balanceChanged = true
            node.eventHandled()
            break
        }
        Thread.sleep(100)
    }

    assertTrue("Should receive balance change event", balanceChanged)
}
```

### 3. Test Sync Events
```kotlin
@Test
fun testSyncCompletedEvent() {
    node.syncWallets()

    var syncCompleted = false
    for (i in 0..20) {
        val event = node.nextEvent()
        if (event is Event.SyncCompleted) {
            assertEquals(SyncType.ONCHAIN_WALLET, event.syncType)
            assertTrue(event.syncedBlockHeight > 0u)
            syncCompleted = true
            node.eventHandled()
            break
        }
        Thread.sleep(100)
    }

    assertTrue("Should receive sync completed event", syncCompleted)
}
```

### 4. Simulate Reorg Scenario
```swift
func testReorgHandling() {
    // This is conceptual - actual reorg testing requires regtest setup

    // 1. Receive transaction
    let txid = receiveTestTransaction()

    // 2. Wait for confirmation event
    waitForEvent { event in
        if case .onchainTransactionConfirmed(let id, _, _, _, _) = event {
            return id == txid
        }
        return false
    }

    // 3. Simulate reorg (in regtest)
    // simulateReorg()

    // 4. Should receive unconfirmed event
    waitForEvent { event in
        if case .onchainTransactionUnconfirmed(let id) = event {
            XCTAssertEqual(id, txid)
            return true
        }
        return false
    }
}
```

---

## Performance Considerations

### Battery Usage
- Events use **significantly less battery** than polling
- Event checking (100ms interval) uses minimal CPU when no events
- Background sync runs automatically every ~30 seconds

### Memory Usage
- Events are queued internally until handled
- Always call `eventHandled()` to free memory
- Don't accumulate events without processing

### Network Usage
- Background sync is automatic - no need for manual sync calls
- Events arrive even when app is backgrounded (if node keeps running)
- Sync frequency is optimized for battery/data usage

---

## Troubleshooting

### Events Not Arriving?
1. Ensure node is started: `node.start()`
2. Check background sync is enabled (default)
3. Verify event loop is running
4. Check logs for sync errors

### Duplicate Events?
- Always call `eventHandled()` after processing
- Don't process the same event multiple times
- Events are queued until marked handled

### Missing Balance Updates?
- Balance events only emit when balance actually changes
- Check both onchain and Lightning balances
- Ensure wallet sync completed first

---

## Support and Resources

- **GitHub Issues**: Report bugs at https://github.com/lightningdevkit/ldk-node
- **API Documentation**: Full API docs for each platform
- **Example Apps**: Check the examples/ directory for complete implementations
- **Community**: LDK Discord for questions and support

---

## Summary

The new event system provides:
- ✅ **Real-time notifications** without polling
- ✅ **Better battery life** on mobile devices
- ✅ **Instant UI updates** for better UX
- ✅ **Automatic background sync** with progress tracking
- ✅ **Reorg handling** for blockchain reorganizations
- ✅ **Type-safe events** in both Swift and Kotlin

Start with the basic event loop, handle the events you need, and enjoy a more responsive, battery-efficient Lightning wallet!