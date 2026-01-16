

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

import com.sun.jna.Library
import com.sun.jna.Native
import com.sun.jna.Structure
import android.os.Build
import androidx.annotation.RequiresApi
import kotlin.coroutines.resume
import kotlinx.coroutines.CancellableContinuation
import kotlinx.coroutines.DelicateCoroutinesApi
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.GlobalScope
import kotlinx.coroutines.Job
import kotlinx.coroutines.launch
import kotlinx.coroutines.suspendCancellableCoroutine
import kotlinx.coroutines.withContext


internal typealias Pointer = com.sun.jna.Pointer
internal val NullPointer: Pointer? = com.sun.jna.Pointer.NULL
internal fun Pointer.toLong(): Long = Pointer.nativeValue(this)
internal fun kotlin.Long.toPointer() = com.sun.jna.Pointer(this)


@kotlin.jvm.JvmInline
value class ByteBuffer(private val inner: java.nio.ByteBuffer) {
    init {
        inner.order(java.nio.ByteOrder.BIG_ENDIAN)
    }

    fun internal() = inner

    fun limit() = inner.limit()

    fun position() = inner.position()

    fun hasRemaining() = inner.hasRemaining()

    fun get() = inner.get()

    fun get(bytesToRead: Int): ByteArray = ByteArray(bytesToRead).apply(inner::get)

    fun getShort() = inner.getShort()

    fun getInt() = inner.getInt()

    fun getLong() = inner.getLong()

    fun getFloat() = inner.getFloat()

    fun getDouble() = inner.getDouble()



    fun put(value: Byte) {
        inner.put(value)
    }

    fun put(src: ByteArray) {
        inner.put(src)
    }

    fun putShort(value: Short) {
        inner.putShort(value)
    }

    fun putInt(value: Int) {
        inner.putInt(value)
    }

    fun putLong(value: Long) {
        inner.putLong(value)
    }

    fun putFloat(value: Float) {
        inner.putFloat(value)
    }

    fun putDouble(value: Double) {
        inner.putDouble(value)
    }


    fun writeUtf8(value: String) {
        Charsets.UTF_8.newEncoder().run {
            onMalformedInput(java.nio.charset.CodingErrorAction.REPLACE)
            encode(java.nio.CharBuffer.wrap(value), inner, false)
        }
    }
}
fun RustBuffer.setValue(array: RustBufferByValue) {
    this.data = array.data
    this.len = array.len
    this.capacity = array.capacity
}

internal object RustBufferHelper {
    fun allocValue(size: ULong = 0UL): RustBufferByValue = uniffiRustCall { status ->
        // Note: need to convert the size to a `Long` value to make this work with JVM.
        UniffiLib.INSTANCE.ffi_ldk_node_rustbuffer_alloc(size.toLong(), status)
    }.also {
        if(it.data == null) {
            throw RuntimeException("RustBuffer.alloc() returned null data pointer (size=${size})")
        }
    }

    fun free(buf: RustBufferByValue) = uniffiRustCall { status ->
        UniffiLib.INSTANCE.ffi_ldk_node_rustbuffer_free(buf, status)
    }
}

@Structure.FieldOrder("capacity", "len", "data")
open class RustBufferStruct(
    // Note: `capacity` and `len` are actually `ULong` values, but JVM only supports signed values.
    // When dealing with these fields, make sure to call `toULong()`.
    @JvmField internal var capacity: Long,
    @JvmField internal var len: Long,
    @JvmField internal var data: Pointer?,
) : Structure() {
    constructor(): this(0.toLong(), 0.toLong(), null)

    class ByValue(
        capacity: Long,
        len: Long,
        data: Pointer?,
    ): RustBuffer(capacity, len, data), Structure.ByValue {
        constructor(): this(0.toLong(), 0.toLong(), null)
    }

    /**
     * The equivalent of the `*mut RustBuffer` type.
     * Required for callbacks taking in an out pointer.
     *
     * Size is the sum of all values in the struct.
     */
    class ByReference(
        capacity: Long,
        len: Long,
        data: Pointer?,
    ): RustBuffer(capacity, len, data), Structure.ByReference {
        constructor(): this(0.toLong(), 0.toLong(), null)
    }
}

typealias RustBuffer = RustBufferStruct
typealias RustBufferByValue = RustBufferStruct.ByValue

internal fun RustBuffer.asByteBuffer(): ByteBuffer? {
    require(this.len <= Int.MAX_VALUE) {
        val length = this.len
        "cannot handle RustBuffer longer than Int.MAX_VALUE bytes: length is $length"
    }
    return ByteBuffer(data?.getByteBuffer(0L, this.len) ?: return null)
}

internal fun RustBufferByValue.asByteBuffer(): ByteBuffer? {
    require(this.len <= Int.MAX_VALUE) {
        val length = this.len
        "cannot handle RustBuffer longer than Int.MAX_VALUE bytes: length is $length"
    }
    return ByteBuffer(data?.getByteBuffer(0L, this.len) ?: return null)
}

internal class RustBufferByReference : com.sun.jna.ptr.ByReference(16)
internal fun RustBufferByReference.setValue(value: RustBufferByValue) {
    // NOTE: The offsets are as they are in the C-like struct.
    val pointer = getPointer()
    pointer.setLong(0, value.capacity)
    pointer.setLong(8, value.len)
    pointer.setPointer(16, value.data)
}
internal fun RustBufferByReference.getValue(): RustBufferByValue {
    val pointer = getPointer()
    val value = RustBufferByValue()
    value.writeField("capacity", pointer.getLong(0))
    value.writeField("len", pointer.getLong(8))
    value.writeField("data", pointer.getLong(16))
    return value
}



// This is a helper for safely passing byte references into the rust code.
// It's not actually used at the moment, because there aren't many things that you
// can take a direct pointer to in the JVM, and if we're going to copy something
// then we might as well copy it into a `RustBuffer`. But it's here for API
// completeness.

@Structure.FieldOrder("len", "data")
internal open class ForeignBytesStruct : Structure() {
    @JvmField internal var len: Int = 0
    @JvmField internal var data: Pointer? = null

    internal class ByValue : ForeignBytes(), Structure.ByValue
}
internal typealias ForeignBytes = ForeignBytesStruct
internal typealias ForeignBytesByValue = ForeignBytesStruct.ByValue

interface FfiConverter<KotlinType, FfiType> {
    // Convert an FFI type to a Kotlin type
    fun lift(value: FfiType): KotlinType

    // Convert an Kotlin type to an FFI type
    fun lower(value: KotlinType): FfiType

    // Read a Kotlin type from a `ByteBuffer`
    fun read(buf: ByteBuffer): KotlinType

    // Calculate bytes to allocate when creating a `RustBuffer`
    //
    // This must return at least as many bytes as the write() function will
    // write. It can return more bytes than needed, for example when writing
    // Strings we can't know the exact bytes needed until we the UTF-8
    // encoding, so we pessimistically allocate the largest size possible (3
    // bytes per codepoint).  Allocating extra bytes is not really a big deal
    // because the `RustBuffer` is short-lived.
    fun allocationSize(value: KotlinType): ULong

    // Write a Kotlin type to a `ByteBuffer`
    fun write(value: KotlinType, buf: ByteBuffer)

    // Lower a value into a `RustBuffer`
    //
    // This method lowers a value into a `RustBuffer` rather than the normal
    // FfiType.  It's used by the callback interface code.  Callback interface
    // returns are always serialized into a `RustBuffer` regardless of their
    // normal FFI type.
    fun lowerIntoRustBuffer(value: KotlinType): RustBufferByValue {
        val rbuf = RustBufferHelper.allocValue(allocationSize(value))
        val bbuf = rbuf.asByteBuffer()!!
        write(value, bbuf)
        return RustBufferByValue(
            capacity = rbuf.capacity,
            len = bbuf.position().toLong(),
            data = rbuf.data,
        )
    }

    // Lift a value from a `RustBuffer`.
    //
    // This here mostly because of the symmetry with `lowerIntoRustBuffer()`.
    // It's currently only used by the `FfiConverterRustBuffer` class below.
    fun liftFromRustBuffer(rbuf: RustBufferByValue): KotlinType {
        val byteBuf = rbuf.asByteBuffer()!!
        try {
           val item = read(byteBuf)
           if (byteBuf.hasRemaining()) {
               throw RuntimeException("junk remaining in buffer after lifting, something is very wrong!!")
           }
           return item
        } finally {
            RustBufferHelper.free(rbuf)
        }
    }
}

// FfiConverter that uses `RustBuffer` as the FfiType
interface FfiConverterRustBuffer<KotlinType>: FfiConverter<KotlinType, RustBufferByValue> {
    override fun lift(value: RustBufferByValue) = liftFromRustBuffer(value)
    override fun lower(value: KotlinType) = lowerIntoRustBuffer(value)
}

internal const val UNIFFI_CALL_SUCCESS = 0.toByte()
internal const val UNIFFI_CALL_ERROR = 1.toByte()
internal const val UNIFFI_CALL_UNEXPECTED_ERROR = 2.toByte()

// Default Implementations
internal fun UniffiRustCallStatus.isSuccess(): Boolean
    = code == UNIFFI_CALL_SUCCESS

internal fun UniffiRustCallStatus.isError(): Boolean
    = code == UNIFFI_CALL_ERROR

internal fun UniffiRustCallStatus.isPanic(): Boolean
    = code == UNIFFI_CALL_UNEXPECTED_ERROR

internal fun UniffiRustCallStatusByValue.isSuccess(): Boolean
    = code == UNIFFI_CALL_SUCCESS

internal fun UniffiRustCallStatusByValue.isError(): Boolean
    = code == UNIFFI_CALL_ERROR

internal fun UniffiRustCallStatusByValue.isPanic(): Boolean
    = code == UNIFFI_CALL_UNEXPECTED_ERROR

// Each top-level error class has a companion object that can lift the error from the call status's rust buffer
interface UniffiRustCallStatusErrorHandler<E> {
    fun lift(errorBuf: RustBufferByValue): E;
}

// Helpers for calling Rust
// In practice we usually need to be synchronized to call this safely, so it doesn't
// synchronize itself

// Call a rust function that returns a Result<>.  Pass in the Error class companion that corresponds to the Err
internal inline fun <U, E: kotlin.Exception> uniffiRustCallWithError(errorHandler: UniffiRustCallStatusErrorHandler<E>, crossinline callback: (UniffiRustCallStatus) -> U): U {
    return UniffiRustCallStatusHelper.withReference() { status ->
        val returnValue = callback(status)
        uniffiCheckCallStatus(errorHandler, status)
        returnValue
    }
}

// Check `status` and throw an error if the call wasn't successful
internal fun<E: kotlin.Exception> uniffiCheckCallStatus(errorHandler: UniffiRustCallStatusErrorHandler<E>, status: UniffiRustCallStatus) {
    if (status.isSuccess()) {
        return
    } else if (status.isError()) {
        throw errorHandler.lift(status.errorBuf)
    } else if (status.isPanic()) {
        // when the rust code sees a panic, it tries to construct a rustbuffer
        // with the message.  but if that code panics, then it just sends back
        // an empty buffer.
        if (status.errorBuf.len > 0) {
            throw InternalException(FfiConverterString.lift(status.errorBuf))
        } else {
            throw InternalException("Rust panic")
        }
    } else {
        throw InternalException("Unknown rust call status: $status.code")
    }
}

// UniffiRustCallStatusErrorHandler implementation for times when we don't expect a CALL_ERROR
object UniffiNullRustCallStatusErrorHandler: UniffiRustCallStatusErrorHandler<InternalException> {
    override fun lift(errorBuf: RustBufferByValue): InternalException {
        RustBufferHelper.free(errorBuf)
        return InternalException("Unexpected CALL_ERROR")
    }
}

// Call a rust function that returns a plain value
internal inline fun <U> uniffiRustCall(crossinline callback: (UniffiRustCallStatus) -> U): U {
    return uniffiRustCallWithError(UniffiNullRustCallStatusErrorHandler, callback)
}

internal inline fun<T> uniffiTraitInterfaceCall(
    callStatus: UniffiRustCallStatus,
    makeCall: () -> T,
    writeReturn: (T) -> Unit,
) {
    try {
        writeReturn(makeCall())
    } catch(e: kotlin.Exception) {
        callStatus.code = UNIFFI_CALL_UNEXPECTED_ERROR
        callStatus.errorBuf = FfiConverterString.lower(e.toString())
    }
}

internal inline fun<T, reified E: Throwable> uniffiTraitInterfaceCallWithError(
    callStatus: UniffiRustCallStatus,
    makeCall: () -> T,
    writeReturn: (T) -> Unit,
    lowerError: (E) -> RustBufferByValue
) {
    try {
        writeReturn(makeCall())
    } catch(e: kotlin.Exception) {
        if (e is E) {
            callStatus.code = UNIFFI_CALL_ERROR
            callStatus.errorBuf = lowerError(e)
        } else {
            callStatus.code = UNIFFI_CALL_UNEXPECTED_ERROR
            callStatus.errorBuf = FfiConverterString.lower(e.toString())
        }
    }
}

@Structure.FieldOrder("code", "errorBuf")
internal open class UniffiRustCallStatusStruct(
    @JvmField internal var code: Byte,
    @JvmField internal var errorBuf: RustBufferByValue,
) : Structure() {
    constructor(): this(0.toByte(), RustBufferByValue())

    internal class ByValue(
        code: Byte,
        errorBuf: RustBufferByValue,
    ): UniffiRustCallStatusStruct(code, errorBuf), Structure.ByValue {
        constructor(): this(0.toByte(), RustBufferByValue())
    }
    internal class ByReference(
        code: Byte,
        errorBuf: RustBufferByValue,
    ): UniffiRustCallStatusStruct(code, errorBuf), Structure.ByReference {
        constructor(): this(0.toByte(), RustBufferByValue())
    }
}

internal typealias UniffiRustCallStatus = UniffiRustCallStatusStruct.ByReference
internal typealias UniffiRustCallStatusByValue = UniffiRustCallStatusStruct.ByValue

internal object UniffiRustCallStatusHelper {
    fun allocValue() = UniffiRustCallStatusByValue()
    fun <U> withReference(block: (UniffiRustCallStatus) -> U): U {
        val status = UniffiRustCallStatus()
        return block(status)
    }
}

internal class UniffiHandleMap<T: Any> {
    private val map = java.util.concurrent.ConcurrentHashMap<Long, T>()
    private val counter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    val size: Int
        get() = map.size

    // Insert a new object into the handle map and get a handle for it
    fun insert(obj: T): Long {
        val handle = counter.getAndAdd(1)
        map[handle] = obj
        return handle
    }

    // Get an object from the handle map
    fun get(handle: Long): T {
        return map[handle] ?: throw InternalException("UniffiHandleMap.get: Invalid handle")
    }

    // Remove an entry from the handlemap and get the Kotlin object back
    fun remove(handle: Long): T {
        return map.remove(handle) ?: throw InternalException("UniffiHandleMap.remove: Invalid handle")
    }
}

typealias ByteByReference = com.sun.jna.ptr.ByteByReference

typealias DoubleByReference = com.sun.jna.ptr.DoubleByReference

typealias FloatByReference = com.sun.jna.ptr.FloatByReference

typealias IntByReference = com.sun.jna.ptr.IntByReference

typealias LongByReference = com.sun.jna.ptr.LongByReference

typealias PointerByReference = com.sun.jna.ptr.PointerByReference

typealias ShortByReference = com.sun.jna.ptr.ShortByReference

// Contains loading, initialization code,
// and the FFI Function declarations in a com.sun.jna.Library.

// Define FFI callback types
internal interface UniffiRustFutureContinuationCallback: com.sun.jna.Callback {
    fun callback(`data`: Long,`pollResult`: Byte,)
}
internal interface UniffiForeignFutureFree: com.sun.jna.Callback {
    fun callback(`handle`: Long,)
}
internal interface UniffiCallbackInterfaceFree: com.sun.jna.Callback {
    fun callback(`handle`: Long,)
}
@Structure.FieldOrder("handle", "free")
internal open class UniffiForeignFutureStruct(
    @JvmField internal var `handle`: Long,
    @JvmField internal var `free`: UniffiForeignFutureFree?,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `handle` = 0.toLong(),
        
        `free` = null,
        
    )

    internal class UniffiByValue(
        `handle`: Long,
        `free`: UniffiForeignFutureFree?,
    ): UniffiForeignFuture(`handle`,`free`,), Structure.ByValue
}

internal typealias UniffiForeignFuture = UniffiForeignFutureStruct

internal fun UniffiForeignFuture.uniffiSetValue(other: UniffiForeignFuture) {
    `handle` = other.`handle`
    `free` = other.`free`
}
internal fun UniffiForeignFuture.uniffiSetValue(other: UniffiForeignFutureUniffiByValue) {
    `handle` = other.`handle`
    `free` = other.`free`
}

internal typealias UniffiForeignFutureUniffiByValue = UniffiForeignFutureStruct.UniffiByValue
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructU8Struct(
    @JvmField internal var `returnValue`: Byte,
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `returnValue` = 0.toByte(),
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `returnValue`: Byte,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructU8(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructU8 = UniffiForeignFutureStructU8Struct

internal fun UniffiForeignFutureStructU8.uniffiSetValue(other: UniffiForeignFutureStructU8) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructU8.uniffiSetValue(other: UniffiForeignFutureStructU8UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructU8UniffiByValue = UniffiForeignFutureStructU8Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteU8: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructU8UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructI8Struct(
    @JvmField internal var `returnValue`: Byte,
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `returnValue` = 0.toByte(),
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `returnValue`: Byte,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructI8(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructI8 = UniffiForeignFutureStructI8Struct

internal fun UniffiForeignFutureStructI8.uniffiSetValue(other: UniffiForeignFutureStructI8) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructI8.uniffiSetValue(other: UniffiForeignFutureStructI8UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructI8UniffiByValue = UniffiForeignFutureStructI8Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteI8: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructI8UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructU16Struct(
    @JvmField internal var `returnValue`: Short,
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `returnValue` = 0.toShort(),
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `returnValue`: Short,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructU16(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructU16 = UniffiForeignFutureStructU16Struct

internal fun UniffiForeignFutureStructU16.uniffiSetValue(other: UniffiForeignFutureStructU16) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructU16.uniffiSetValue(other: UniffiForeignFutureStructU16UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructU16UniffiByValue = UniffiForeignFutureStructU16Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteU16: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructU16UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructI16Struct(
    @JvmField internal var `returnValue`: Short,
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `returnValue` = 0.toShort(),
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `returnValue`: Short,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructI16(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructI16 = UniffiForeignFutureStructI16Struct

internal fun UniffiForeignFutureStructI16.uniffiSetValue(other: UniffiForeignFutureStructI16) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructI16.uniffiSetValue(other: UniffiForeignFutureStructI16UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructI16UniffiByValue = UniffiForeignFutureStructI16Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteI16: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructI16UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructU32Struct(
    @JvmField internal var `returnValue`: Int,
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `returnValue` = 0,
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `returnValue`: Int,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructU32(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructU32 = UniffiForeignFutureStructU32Struct

internal fun UniffiForeignFutureStructU32.uniffiSetValue(other: UniffiForeignFutureStructU32) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructU32.uniffiSetValue(other: UniffiForeignFutureStructU32UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructU32UniffiByValue = UniffiForeignFutureStructU32Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteU32: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructU32UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructI32Struct(
    @JvmField internal var `returnValue`: Int,
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `returnValue` = 0,
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `returnValue`: Int,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructI32(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructI32 = UniffiForeignFutureStructI32Struct

internal fun UniffiForeignFutureStructI32.uniffiSetValue(other: UniffiForeignFutureStructI32) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructI32.uniffiSetValue(other: UniffiForeignFutureStructI32UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructI32UniffiByValue = UniffiForeignFutureStructI32Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteI32: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructI32UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructU64Struct(
    @JvmField internal var `returnValue`: Long,
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `returnValue` = 0.toLong(),
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `returnValue`: Long,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructU64(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructU64 = UniffiForeignFutureStructU64Struct

internal fun UniffiForeignFutureStructU64.uniffiSetValue(other: UniffiForeignFutureStructU64) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructU64.uniffiSetValue(other: UniffiForeignFutureStructU64UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructU64UniffiByValue = UniffiForeignFutureStructU64Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteU64: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructU64UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructI64Struct(
    @JvmField internal var `returnValue`: Long,
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `returnValue` = 0.toLong(),
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `returnValue`: Long,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructI64(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructI64 = UniffiForeignFutureStructI64Struct

internal fun UniffiForeignFutureStructI64.uniffiSetValue(other: UniffiForeignFutureStructI64) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructI64.uniffiSetValue(other: UniffiForeignFutureStructI64UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructI64UniffiByValue = UniffiForeignFutureStructI64Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteI64: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructI64UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructF32Struct(
    @JvmField internal var `returnValue`: Float,
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `returnValue` = 0.0f,
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `returnValue`: Float,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructF32(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructF32 = UniffiForeignFutureStructF32Struct

internal fun UniffiForeignFutureStructF32.uniffiSetValue(other: UniffiForeignFutureStructF32) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructF32.uniffiSetValue(other: UniffiForeignFutureStructF32UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructF32UniffiByValue = UniffiForeignFutureStructF32Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteF32: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructF32UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructF64Struct(
    @JvmField internal var `returnValue`: Double,
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `returnValue` = 0.0,
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `returnValue`: Double,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructF64(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructF64 = UniffiForeignFutureStructF64Struct

internal fun UniffiForeignFutureStructF64.uniffiSetValue(other: UniffiForeignFutureStructF64) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructF64.uniffiSetValue(other: UniffiForeignFutureStructF64UniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructF64UniffiByValue = UniffiForeignFutureStructF64Struct.UniffiByValue
internal interface UniffiForeignFutureCompleteF64: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructF64UniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructPointerStruct(
    @JvmField internal var `returnValue`: Pointer?,
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `returnValue` = NullPointer,
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `returnValue`: Pointer?,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructPointer(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructPointer = UniffiForeignFutureStructPointerStruct

internal fun UniffiForeignFutureStructPointer.uniffiSetValue(other: UniffiForeignFutureStructPointer) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructPointer.uniffiSetValue(other: UniffiForeignFutureStructPointerUniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructPointerUniffiByValue = UniffiForeignFutureStructPointerStruct.UniffiByValue
internal interface UniffiForeignFutureCompletePointer: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructPointerUniffiByValue,)
}
@Structure.FieldOrder("returnValue", "callStatus")
internal open class UniffiForeignFutureStructRustBufferStruct(
    @JvmField internal var `returnValue`: RustBufferByValue,
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `returnValue` = RustBufferHelper.allocValue(),
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `returnValue`: RustBufferByValue,
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructRustBuffer(`returnValue`,`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructRustBuffer = UniffiForeignFutureStructRustBufferStruct

internal fun UniffiForeignFutureStructRustBuffer.uniffiSetValue(other: UniffiForeignFutureStructRustBuffer) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructRustBuffer.uniffiSetValue(other: UniffiForeignFutureStructRustBufferUniffiByValue) {
    `returnValue` = other.`returnValue`
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructRustBufferUniffiByValue = UniffiForeignFutureStructRustBufferStruct.UniffiByValue
internal interface UniffiForeignFutureCompleteRustBuffer: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructRustBufferUniffiByValue,)
}
@Structure.FieldOrder("callStatus")
internal open class UniffiForeignFutureStructVoidStruct(
    @JvmField internal var `callStatus`: UniffiRustCallStatusByValue,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `callStatus` = UniffiRustCallStatusHelper.allocValue(),
        
    )

    internal class UniffiByValue(
        `callStatus`: UniffiRustCallStatusByValue,
    ): UniffiForeignFutureStructVoid(`callStatus`,), Structure.ByValue
}

internal typealias UniffiForeignFutureStructVoid = UniffiForeignFutureStructVoidStruct

internal fun UniffiForeignFutureStructVoid.uniffiSetValue(other: UniffiForeignFutureStructVoid) {
    `callStatus` = other.`callStatus`
}
internal fun UniffiForeignFutureStructVoid.uniffiSetValue(other: UniffiForeignFutureStructVoidUniffiByValue) {
    `callStatus` = other.`callStatus`
}

internal typealias UniffiForeignFutureStructVoidUniffiByValue = UniffiForeignFutureStructVoidStruct.UniffiByValue
internal interface UniffiForeignFutureCompleteVoid: com.sun.jna.Callback {
    fun callback(`callbackData`: Long,`result`: UniffiForeignFutureStructVoidUniffiByValue,)
}
internal interface UniffiCallbackInterfaceLogWriterMethod0: com.sun.jna.Callback {
    fun callback(`uniffiHandle`: Long,`record`: RustBufferByValue,`uniffiOutReturn`: Pointer,uniffiCallStatus: UniffiRustCallStatus,)
}
internal interface UniffiCallbackInterfaceVssHeaderProviderMethod0: com.sun.jna.Callback {
    fun callback(`uniffiHandle`: Long,`request`: RustBufferByValue,`uniffiFutureCallback`: UniffiForeignFutureCompleteRustBuffer,`uniffiCallbackData`: Long,`uniffiOutReturn`: UniffiForeignFuture,)
}
@Structure.FieldOrder("log", "uniffiFree")
internal open class UniffiVTableCallbackInterfaceLogWriterStruct(
    @JvmField internal var `log`: UniffiCallbackInterfaceLogWriterMethod0?,
    @JvmField internal var `uniffiFree`: UniffiCallbackInterfaceFree?,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `log` = null,
        
        `uniffiFree` = null,
        
    )

    internal class UniffiByValue(
        `log`: UniffiCallbackInterfaceLogWriterMethod0?,
        `uniffiFree`: UniffiCallbackInterfaceFree?,
    ): UniffiVTableCallbackInterfaceLogWriter(`log`,`uniffiFree`,), Structure.ByValue
}

internal typealias UniffiVTableCallbackInterfaceLogWriter = UniffiVTableCallbackInterfaceLogWriterStruct

internal fun UniffiVTableCallbackInterfaceLogWriter.uniffiSetValue(other: UniffiVTableCallbackInterfaceLogWriter) {
    `log` = other.`log`
    `uniffiFree` = other.`uniffiFree`
}
internal fun UniffiVTableCallbackInterfaceLogWriter.uniffiSetValue(other: UniffiVTableCallbackInterfaceLogWriterUniffiByValue) {
    `log` = other.`log`
    `uniffiFree` = other.`uniffiFree`
}

internal typealias UniffiVTableCallbackInterfaceLogWriterUniffiByValue = UniffiVTableCallbackInterfaceLogWriterStruct.UniffiByValue
@Structure.FieldOrder("getHeaders", "uniffiFree")
internal open class UniffiVTableCallbackInterfaceVssHeaderProviderStruct(
    @JvmField internal var `getHeaders`: UniffiCallbackInterfaceVssHeaderProviderMethod0?,
    @JvmField internal var `uniffiFree`: UniffiCallbackInterfaceFree?,
) : com.sun.jna.Structure() {
    constructor(): this(
        
        `getHeaders` = null,
        
        `uniffiFree` = null,
        
    )

    internal class UniffiByValue(
        `getHeaders`: UniffiCallbackInterfaceVssHeaderProviderMethod0?,
        `uniffiFree`: UniffiCallbackInterfaceFree?,
    ): UniffiVTableCallbackInterfaceVssHeaderProvider(`getHeaders`,`uniffiFree`,), Structure.ByValue
}

internal typealias UniffiVTableCallbackInterfaceVssHeaderProvider = UniffiVTableCallbackInterfaceVssHeaderProviderStruct

internal fun UniffiVTableCallbackInterfaceVssHeaderProvider.uniffiSetValue(other: UniffiVTableCallbackInterfaceVssHeaderProvider) {
    `getHeaders` = other.`getHeaders`
    `uniffiFree` = other.`uniffiFree`
}
internal fun UniffiVTableCallbackInterfaceVssHeaderProvider.uniffiSetValue(other: UniffiVTableCallbackInterfaceVssHeaderProviderUniffiByValue) {
    `getHeaders` = other.`getHeaders`
    `uniffiFree` = other.`uniffiFree`
}

internal typealias UniffiVTableCallbackInterfaceVssHeaderProviderUniffiByValue = UniffiVTableCallbackInterfaceVssHeaderProviderStruct.UniffiByValue











































































































































































































































































































































































































































































































@Synchronized
private fun findLibraryName(componentName: String): String {
    val libOverride = System.getProperty("uniffi.component.$componentName.libraryOverride")
    if (libOverride != null) {
        return libOverride
    }
    return "ldk_node"
}

private inline fun <reified Lib : Library> loadIndirect(
    componentName: String
): Lib {
    return Native.load<Lib>(findLibraryName(componentName), Lib::class.java)
}

// A JNA Library to expose the extern-C FFI definitions.
// This is an implementation detail which will be called internally by the public API.

internal interface UniffiLib : Library {
    companion object {
        internal val INSTANCE: UniffiLib by lazy {
            loadIndirect<UniffiLib>(componentName = "ldk_node")
                .also { lib: UniffiLib ->
                    uniffiCheckContractApiVersion(lib)
                    uniffiCheckApiChecksums(lib)
                    uniffiCallbackInterfaceLogWriter.register(lib)
                    }
        }
        
        // The Cleaner for the whole library
        internal val CLEANER: UniffiCleaner by lazy {
            UniffiCleaner.create()
        }
    }

    fun uniffi_ldk_node_fn_clone_bolt11invoice(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_bolt11invoice(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_constructor_bolt11invoice_from_str(
        `invoiceStr`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt11invoice_amount_milli_satoshis(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11invoice_currency(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11invoice_expiry_time_seconds(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_bolt11invoice_fallback_addresses(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11invoice_invoice_description(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11invoice_is_expired(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_method_bolt11invoice_min_final_cltv_expiry_delta(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_bolt11invoice_network(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11invoice_payment_hash(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11invoice_payment_secret(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11invoice_recover_payee_pub_key(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11invoice_route_hints(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11invoice_seconds_since_epoch(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_bolt11invoice_seconds_until_expiry(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_bolt11invoice_signable_hash(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11invoice_would_expire(
        `ptr`: Pointer?,
        `atTimeSeconds`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_method_bolt11invoice_uniffi_trait_debug(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11invoice_uniffi_trait_display(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11invoice_uniffi_trait_eq_eq(
        `ptr`: Pointer?,
        `other`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_method_bolt11invoice_uniffi_trait_eq_ne(
        `ptr`: Pointer?,
        `other`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_clone_bolt11payment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_bolt11payment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_bolt11payment_claim_for_hash(
        `ptr`: Pointer?,
        `paymentHash`: RustBufferByValue,
        `claimableAmountMsat`: Long,
        `preimage`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_bolt11payment_estimate_routing_fees(
        `ptr`: Pointer?,
        `invoice`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_bolt11payment_estimate_routing_fees_using_amount(
        `ptr`: Pointer?,
        `invoice`: Pointer?,
        `amountMsat`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_bolt11payment_fail_for_hash(
        `ptr`: Pointer?,
        `paymentHash`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_bolt11payment_receive(
        `ptr`: Pointer?,
        `amountMsat`: Long,
        `description`: RustBufferByValue,
        `expirySecs`: Int,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt11payment_receive_for_hash(
        `ptr`: Pointer?,
        `amountMsat`: Long,
        `description`: RustBufferByValue,
        `expirySecs`: Int,
        `paymentHash`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt11payment_receive_variable_amount(
        `ptr`: Pointer?,
        `description`: RustBufferByValue,
        `expirySecs`: Int,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt11payment_receive_variable_amount_for_hash(
        `ptr`: Pointer?,
        `description`: RustBufferByValue,
        `expirySecs`: Int,
        `paymentHash`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt11payment_receive_variable_amount_via_jit_channel(
        `ptr`: Pointer?,
        `description`: RustBufferByValue,
        `expirySecs`: Int,
        `maxProportionalLspFeeLimitPpmMsat`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt11payment_receive_variable_amount_via_jit_channel_for_hash(
        `ptr`: Pointer?,
        `description`: RustBufferByValue,
        `expirySecs`: Int,
        `maxProportionalLspFeeLimitPpmMsat`: RustBufferByValue,
        `paymentHash`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt11payment_receive_via_jit_channel(
        `ptr`: Pointer?,
        `amountMsat`: Long,
        `description`: RustBufferByValue,
        `expirySecs`: Int,
        `maxLspFeeLimitMsat`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt11payment_receive_via_jit_channel_for_hash(
        `ptr`: Pointer?,
        `amountMsat`: Long,
        `description`: RustBufferByValue,
        `expirySecs`: Int,
        `maxLspFeeLimitMsat`: RustBufferByValue,
        `paymentHash`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt11payment_send(
        `ptr`: Pointer?,
        `invoice`: Pointer?,
        `routeParameters`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt11payment_send_probes(
        `ptr`: Pointer?,
        `invoice`: Pointer?,
        `routeParameters`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_bolt11payment_send_probes_using_amount(
        `ptr`: Pointer?,
        `invoice`: Pointer?,
        `amountMsat`: Long,
        `routeParameters`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_bolt11payment_send_using_amount(
        `ptr`: Pointer?,
        `invoice`: Pointer?,
        `amountMsat`: Long,
        `routeParameters`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_clone_bolt12invoice(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_bolt12invoice(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_constructor_bolt12invoice_from_str(
        `invoiceStr`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt12invoice_absolute_expiry_seconds(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_amount(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_amount_msats(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_bolt12invoice_chain(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_created_at(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_bolt12invoice_encode(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_fallback_addresses(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_invoice_description(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_is_expired(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_method_bolt12invoice_issuer(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_issuer_signing_pubkey(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_metadata(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_offer_chains(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_payer_note(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_payer_signing_pubkey(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_payment_hash(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_quantity(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_relative_expiry(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_bolt12invoice_signable_hash(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12invoice_signing_pubkey(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_clone_bolt12payment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_bolt12payment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_bolt12payment_blinded_paths_for_async_recipient(
        `ptr`: Pointer?,
        `recipientId`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12payment_initiate_refund(
        `ptr`: Pointer?,
        `amountMsat`: Long,
        `expirySecs`: Int,
        `quantity`: RustBufferByValue,
        `payerNote`: RustBufferByValue,
        `routeParameters`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt12payment_receive(
        `ptr`: Pointer?,
        `amountMsat`: Long,
        `description`: RustBufferByValue,
        `expirySecs`: RustBufferByValue,
        `quantity`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt12payment_receive_async(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt12payment_receive_variable_amount(
        `ptr`: Pointer?,
        `description`: RustBufferByValue,
        `expirySecs`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt12payment_request_refund_payment(
        `ptr`: Pointer?,
        `refund`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_bolt12payment_send(
        `ptr`: Pointer?,
        `offer`: Pointer?,
        `quantity`: RustBufferByValue,
        `payerNote`: RustBufferByValue,
        `routeParameters`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12payment_send_using_amount(
        `ptr`: Pointer?,
        `offer`: Pointer?,
        `amountMsat`: Long,
        `quantity`: RustBufferByValue,
        `payerNote`: RustBufferByValue,
        `routeParameters`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_bolt12payment_set_paths_to_static_invoice_server(
        `ptr`: Pointer?,
        `paths`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_clone_builder(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_builder(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_constructor_builder_from_config(
        `config`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_constructor_builder_new(
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_builder_build(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_builder_build_with_fs_store(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_builder_build_with_vss_store(
        `ptr`: Pointer?,
        `vssUrl`: RustBufferByValue,
        `storeId`: RustBufferByValue,
        `lnurlAuthServerUrl`: RustBufferByValue,
        `fixedHeaders`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_builder_build_with_vss_store_and_fixed_headers(
        `ptr`: Pointer?,
        `vssUrl`: RustBufferByValue,
        `storeId`: RustBufferByValue,
        `fixedHeaders`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_builder_build_with_vss_store_and_header_provider(
        `ptr`: Pointer?,
        `vssUrl`: RustBufferByValue,
        `storeId`: RustBufferByValue,
        `headerProvider`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_builder_set_announcement_addresses(
        `ptr`: Pointer?,
        `announcementAddresses`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_async_payments_role(
        `ptr`: Pointer?,
        `role`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_chain_source_bitcoind_rest(
        `ptr`: Pointer?,
        `restHost`: RustBufferByValue,
        `restPort`: Short,
        `rpcHost`: RustBufferByValue,
        `rpcPort`: Short,
        `rpcUser`: RustBufferByValue,
        `rpcPassword`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_chain_source_bitcoind_rpc(
        `ptr`: Pointer?,
        `rpcHost`: RustBufferByValue,
        `rpcPort`: Short,
        `rpcUser`: RustBufferByValue,
        `rpcPassword`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_chain_source_electrum(
        `ptr`: Pointer?,
        `serverUrl`: RustBufferByValue,
        `config`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_chain_source_esplora(
        `ptr`: Pointer?,
        `serverUrl`: RustBufferByValue,
        `config`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_channel_data_migration(
        `ptr`: Pointer?,
        `migration`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_custom_logger(
        `ptr`: Pointer?,
        `logWriter`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_entropy_bip39_mnemonic(
        `ptr`: Pointer?,
        `mnemonic`: RustBufferByValue,
        `passphrase`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_entropy_seed_bytes(
        `ptr`: Pointer?,
        `seedBytes`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_entropy_seed_path(
        `ptr`: Pointer?,
        `seedPath`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_filesystem_logger(
        `ptr`: Pointer?,
        `logFilePath`: RustBufferByValue,
        `maxLogLevel`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_gossip_source_p2p(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_gossip_source_rgs(
        `ptr`: Pointer?,
        `rgsServerUrl`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_liquidity_source_lsps1(
        `ptr`: Pointer?,
        `nodeId`: RustBufferByValue,
        `address`: RustBufferByValue,
        `token`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_liquidity_source_lsps2(
        `ptr`: Pointer?,
        `nodeId`: RustBufferByValue,
        `address`: RustBufferByValue,
        `token`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_listening_addresses(
        `ptr`: Pointer?,
        `listeningAddresses`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_log_facade_logger(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_network(
        `ptr`: Pointer?,
        `network`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_node_alias(
        `ptr`: Pointer?,
        `nodeAlias`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_pathfinding_scores_source(
        `ptr`: Pointer?,
        `url`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_builder_set_storage_dir_path(
        `ptr`: Pointer?,
        `storageDirPath`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_clone_feerate(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_feerate(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_constructor_feerate_from_sat_per_kwu(
        `satKwu`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_constructor_feerate_from_sat_per_vb_unchecked(
        `satVb`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_feerate_to_sat_per_kwu(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_feerate_to_sat_per_vb_ceil(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_feerate_to_sat_per_vb_floor(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_clone_lsps1liquidity(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_lsps1liquidity(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_lsps1liquidity_check_order_status(
        `ptr`: Pointer?,
        `orderId`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_lsps1liquidity_request_channel(
        `ptr`: Pointer?,
        `lspBalanceSat`: Long,
        `clientBalanceSat`: Long,
        `channelExpiryBlocks`: Int,
        `announceChannel`: Byte,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_clone_logwriter(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_logwriter(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_init_callback_vtable_logwriter(
        `vtable`: UniffiVTableCallbackInterfaceLogWriter,
    ): Unit
    fun uniffi_ldk_node_fn_method_logwriter_log(
        `ptr`: Pointer?,
        `record`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_clone_networkgraph(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_networkgraph(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_networkgraph_channel(
        `ptr`: Pointer?,
        `shortChannelId`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_networkgraph_list_channels(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_networkgraph_list_nodes(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_networkgraph_node(
        `ptr`: Pointer?,
        `nodeId`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_clone_node(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_node(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_announcement_addresses(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_bolt11_payment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_node_bolt12_payment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_node_close_channel(
        `ptr`: Pointer?,
        `userChannelId`: RustBufferByValue,
        `counterpartyNodeId`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_config(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_connect(
        `ptr`: Pointer?,
        `nodeId`: RustBufferByValue,
        `address`: RustBufferByValue,
        `persist`: Byte,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_disconnect(
        `ptr`: Pointer?,
        `nodeId`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_event_handled(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_export_pathfinding_scores(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_force_close_channel(
        `ptr`: Pointer?,
        `userChannelId`: RustBufferByValue,
        `counterpartyNodeId`: RustBufferByValue,
        `reason`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_get_address_balance(
        `ptr`: Pointer?,
        `addressStr`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_node_get_transaction_details(
        `ptr`: Pointer?,
        `txid`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_list_balances(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_list_channels(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_list_payments(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_list_peers(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_listening_addresses(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_lsps1_liquidity(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_node_network_graph(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_node_next_event(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_next_event_async(
        `ptr`: Pointer?,
    ): Long
    fun uniffi_ldk_node_fn_method_node_node_alias(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_node_id(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_onchain_payment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_node_open_announced_channel(
        `ptr`: Pointer?,
        `nodeId`: RustBufferByValue,
        `address`: RustBufferByValue,
        `channelAmountSats`: Long,
        `pushToCounterpartyMsat`: RustBufferByValue,
        `channelConfig`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_open_channel(
        `ptr`: Pointer?,
        `nodeId`: RustBufferByValue,
        `address`: RustBufferByValue,
        `channelAmountSats`: Long,
        `pushToCounterpartyMsat`: RustBufferByValue,
        `channelConfig`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_payment(
        `ptr`: Pointer?,
        `paymentId`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_remove_payment(
        `ptr`: Pointer?,
        `paymentId`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_sign_message(
        `ptr`: Pointer?,
        `msg`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_splice_in(
        `ptr`: Pointer?,
        `userChannelId`: RustBufferByValue,
        `counterpartyNodeId`: RustBufferByValue,
        `spliceAmountSats`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_splice_out(
        `ptr`: Pointer?,
        `userChannelId`: RustBufferByValue,
        `counterpartyNodeId`: RustBufferByValue,
        `address`: RustBufferByValue,
        `spliceAmountSats`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_spontaneous_payment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_node_start(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_status(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_node_stop(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_sync_wallets(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_unified_qr_payment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_node_update_channel_config(
        `ptr`: Pointer?,
        `userChannelId`: RustBufferByValue,
        `counterpartyNodeId`: RustBufferByValue,
        `channelConfig`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_node_verify_signature(
        `ptr`: Pointer?,
        `msg`: RustBufferByValue,
        `sig`: RustBufferByValue,
        `pkey`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_method_node_wait_next_event(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_clone_offer(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_offer(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_constructor_offer_from_str(
        `offerStr`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_offer_absolute_expiry_seconds(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_offer_amount(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_offer_chains(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_offer_expects_quantity(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_method_offer_id(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_offer_is_expired(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_method_offer_is_valid_quantity(
        `ptr`: Pointer?,
        `quantity`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_method_offer_issuer(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_offer_issuer_signing_pubkey(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_offer_metadata(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_offer_offer_description(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_offer_supports_chain(
        `ptr`: Pointer?,
        `chain`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_method_offer_uniffi_trait_debug(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_offer_uniffi_trait_display(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_offer_uniffi_trait_eq_eq(
        `ptr`: Pointer?,
        `other`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_method_offer_uniffi_trait_eq_ne(
        `ptr`: Pointer?,
        `other`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_clone_onchainpayment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_onchainpayment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_onchainpayment_accelerate_by_cpfp(
        `ptr`: Pointer?,
        `txid`: RustBufferByValue,
        `feeRate`: RustBufferByValue,
        `destinationAddress`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_onchainpayment_bump_fee_by_rbf(
        `ptr`: Pointer?,
        `txid`: RustBufferByValue,
        `feeRate`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_onchainpayment_calculate_cpfp_fee_rate(
        `ptr`: Pointer?,
        `parentTxid`: RustBufferByValue,
        `urgent`: Byte,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_onchainpayment_calculate_total_fee(
        `ptr`: Pointer?,
        `address`: RustBufferByValue,
        `amountSats`: Long,
        `feeRate`: RustBufferByValue,
        `utxosToSpend`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_onchainpayment_list_spendable_outputs(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_onchainpayment_new_address(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_onchainpayment_select_utxos_with_algorithm(
        `ptr`: Pointer?,
        `targetAmountSats`: Long,
        `feeRate`: RustBufferByValue,
        `algorithm`: RustBufferByValue,
        `utxos`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_onchainpayment_send_all_to_address(
        `ptr`: Pointer?,
        `address`: RustBufferByValue,
        `retainReserve`: Byte,
        `feeRate`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_onchainpayment_send_to_address(
        `ptr`: Pointer?,
        `address`: RustBufferByValue,
        `amountSats`: Long,
        `feeRate`: RustBufferByValue,
        `utxosToSpend`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_clone_refund(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_refund(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_constructor_refund_from_str(
        `refundStr`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_method_refund_absolute_expiry_seconds(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_refund_amount_msats(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun uniffi_ldk_node_fn_method_refund_chain(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_refund_is_expired(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_method_refund_issuer(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_refund_payer_metadata(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_refund_payer_note(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_refund_payer_signing_pubkey(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_refund_quantity(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_refund_refund_description(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_refund_uniffi_trait_debug(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_refund_uniffi_trait_display(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_refund_uniffi_trait_eq_eq(
        `ptr`: Pointer?,
        `other`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_method_refund_uniffi_trait_eq_ne(
        `ptr`: Pointer?,
        `other`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun uniffi_ldk_node_fn_clone_spontaneouspayment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_spontaneouspayment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_spontaneouspayment_send(
        `ptr`: Pointer?,
        `amountMsat`: Long,
        `nodeId`: RustBufferByValue,
        `routeParameters`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_spontaneouspayment_send_probes(
        `ptr`: Pointer?,
        `amountMsat`: Long,
        `nodeId`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_spontaneouspayment_send_with_custom_tlvs(
        `ptr`: Pointer?,
        `amountMsat`: Long,
        `nodeId`: RustBufferByValue,
        `routeParameters`: RustBufferByValue,
        `customTlvs`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_spontaneouspayment_send_with_preimage(
        `ptr`: Pointer?,
        `amountMsat`: Long,
        `nodeId`: RustBufferByValue,
        `preimage`: RustBufferByValue,
        `routeParameters`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_spontaneouspayment_send_with_preimage_and_custom_tlvs(
        `ptr`: Pointer?,
        `amountMsat`: Long,
        `nodeId`: RustBufferByValue,
        `customTlvs`: RustBufferByValue,
        `preimage`: RustBufferByValue,
        `routeParameters`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_clone_unifiedqrpayment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_unifiedqrpayment(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_unifiedqrpayment_receive(
        `ptr`: Pointer?,
        `amountSats`: Long,
        `message`: RustBufferByValue,
        `expirySec`: Int,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_method_unifiedqrpayment_send(
        `ptr`: Pointer?,
        `uriStr`: RustBufferByValue,
        `routeParameters`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_clone_vssheaderprovider(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun uniffi_ldk_node_fn_free_vssheaderprovider(
        `ptr`: Pointer?,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_fn_method_vssheaderprovider_get_headers(
        `ptr`: Pointer?,
        `request`: RustBufferByValue,
    ): Long
    fun uniffi_ldk_node_fn_func_default_config(
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_func_derive_node_secret_from_mnemonic(
        `mnemonic`: RustBufferByValue,
        `passphrase`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun uniffi_ldk_node_fn_func_generate_entropy_mnemonic(
        `wordCount`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun ffi_ldk_node_rustbuffer_alloc(
        `size`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun ffi_ldk_node_rustbuffer_from_bytes(
        `bytes`: ForeignBytesByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun ffi_ldk_node_rustbuffer_free(
        `buf`: RustBufferByValue,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun ffi_ldk_node_rustbuffer_reserve(
        `buf`: RustBufferByValue,
        `additional`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun ffi_ldk_node_rust_future_poll_u8(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_u8(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_u8(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_u8(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun ffi_ldk_node_rust_future_poll_i8(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_i8(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_i8(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_i8(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Byte
    fun ffi_ldk_node_rust_future_poll_u16(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_u16(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_u16(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_u16(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Short
    fun ffi_ldk_node_rust_future_poll_i16(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_i16(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_i16(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_i16(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Short
    fun ffi_ldk_node_rust_future_poll_u32(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_u32(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_u32(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_u32(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Int
    fun ffi_ldk_node_rust_future_poll_i32(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_i32(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_i32(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_i32(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Int
    fun ffi_ldk_node_rust_future_poll_u64(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_u64(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_u64(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_u64(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun ffi_ldk_node_rust_future_poll_i64(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_i64(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_i64(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_i64(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Long
    fun ffi_ldk_node_rust_future_poll_f32(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_f32(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_f32(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_f32(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Float
    fun ffi_ldk_node_rust_future_poll_f64(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_f64(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_f64(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_f64(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Double
    fun ffi_ldk_node_rust_future_poll_pointer(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_pointer(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_pointer(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_pointer(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Pointer?
    fun ffi_ldk_node_rust_future_poll_rust_buffer(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_rust_buffer(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_rust_buffer(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_rust_buffer(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): RustBufferByValue
    fun ffi_ldk_node_rust_future_poll_void(
        `handle`: Long,
        `callback`: UniffiRustFutureContinuationCallback,
        `callbackData`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_cancel_void(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_free_void(
        `handle`: Long,
    ): Unit
    fun ffi_ldk_node_rust_future_complete_void(
        `handle`: Long,
        uniffiCallStatus: UniffiRustCallStatus,
    ): Unit
    fun uniffi_ldk_node_checksum_func_default_config(
    ): Short
    fun uniffi_ldk_node_checksum_func_derive_node_secret_from_mnemonic(
    ): Short
    fun uniffi_ldk_node_checksum_func_generate_entropy_mnemonic(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_amount_milli_satoshis(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_currency(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_expiry_time_seconds(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_fallback_addresses(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_invoice_description(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_is_expired(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_min_final_cltv_expiry_delta(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_network(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_payment_hash(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_payment_secret(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_recover_payee_pub_key(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_route_hints(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_seconds_since_epoch(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_seconds_until_expiry(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_signable_hash(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11invoice_would_expire(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_claim_for_hash(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_estimate_routing_fees(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_estimate_routing_fees_using_amount(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_fail_for_hash(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_receive(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_receive_for_hash(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_receive_variable_amount(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_receive_variable_amount_for_hash(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_receive_variable_amount_via_jit_channel(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_receive_variable_amount_via_jit_channel_for_hash(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_receive_via_jit_channel(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_receive_via_jit_channel_for_hash(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_send(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_send_probes(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_send_probes_using_amount(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt11payment_send_using_amount(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_absolute_expiry_seconds(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_amount(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_amount_msats(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_chain(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_created_at(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_encode(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_fallback_addresses(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_invoice_description(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_is_expired(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_issuer(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_issuer_signing_pubkey(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_metadata(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_offer_chains(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_payer_note(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_payer_signing_pubkey(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_payment_hash(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_quantity(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_relative_expiry(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_signable_hash(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12invoice_signing_pubkey(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12payment_blinded_paths_for_async_recipient(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12payment_initiate_refund(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12payment_receive(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12payment_receive_async(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12payment_receive_variable_amount(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12payment_request_refund_payment(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12payment_send(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12payment_send_using_amount(
    ): Short
    fun uniffi_ldk_node_checksum_method_bolt12payment_set_paths_to_static_invoice_server(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_build(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_build_with_fs_store(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_build_with_vss_store(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_build_with_vss_store_and_fixed_headers(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_build_with_vss_store_and_header_provider(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_announcement_addresses(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_async_payments_role(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_chain_source_bitcoind_rest(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_chain_source_bitcoind_rpc(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_chain_source_electrum(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_chain_source_esplora(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_channel_data_migration(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_custom_logger(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_entropy_bip39_mnemonic(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_entropy_seed_bytes(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_entropy_seed_path(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_filesystem_logger(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_gossip_source_p2p(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_gossip_source_rgs(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_liquidity_source_lsps1(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_liquidity_source_lsps2(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_listening_addresses(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_log_facade_logger(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_network(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_node_alias(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_pathfinding_scores_source(
    ): Short
    fun uniffi_ldk_node_checksum_method_builder_set_storage_dir_path(
    ): Short
    fun uniffi_ldk_node_checksum_method_feerate_to_sat_per_kwu(
    ): Short
    fun uniffi_ldk_node_checksum_method_feerate_to_sat_per_vb_ceil(
    ): Short
    fun uniffi_ldk_node_checksum_method_feerate_to_sat_per_vb_floor(
    ): Short
    fun uniffi_ldk_node_checksum_method_lsps1liquidity_check_order_status(
    ): Short
    fun uniffi_ldk_node_checksum_method_lsps1liquidity_request_channel(
    ): Short
    fun uniffi_ldk_node_checksum_method_logwriter_log(
    ): Short
    fun uniffi_ldk_node_checksum_method_networkgraph_channel(
    ): Short
    fun uniffi_ldk_node_checksum_method_networkgraph_list_channels(
    ): Short
    fun uniffi_ldk_node_checksum_method_networkgraph_list_nodes(
    ): Short
    fun uniffi_ldk_node_checksum_method_networkgraph_node(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_announcement_addresses(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_bolt11_payment(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_bolt12_payment(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_close_channel(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_config(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_connect(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_disconnect(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_event_handled(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_export_pathfinding_scores(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_force_close_channel(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_get_address_balance(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_get_transaction_details(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_list_balances(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_list_channels(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_list_payments(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_list_peers(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_listening_addresses(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_lsps1_liquidity(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_network_graph(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_next_event(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_next_event_async(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_node_alias(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_node_id(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_onchain_payment(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_open_announced_channel(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_open_channel(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_payment(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_remove_payment(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_sign_message(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_splice_in(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_splice_out(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_spontaneous_payment(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_start(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_status(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_stop(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_sync_wallets(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_unified_qr_payment(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_update_channel_config(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_verify_signature(
    ): Short
    fun uniffi_ldk_node_checksum_method_node_wait_next_event(
    ): Short
    fun uniffi_ldk_node_checksum_method_offer_absolute_expiry_seconds(
    ): Short
    fun uniffi_ldk_node_checksum_method_offer_amount(
    ): Short
    fun uniffi_ldk_node_checksum_method_offer_chains(
    ): Short
    fun uniffi_ldk_node_checksum_method_offer_expects_quantity(
    ): Short
    fun uniffi_ldk_node_checksum_method_offer_id(
    ): Short
    fun uniffi_ldk_node_checksum_method_offer_is_expired(
    ): Short
    fun uniffi_ldk_node_checksum_method_offer_is_valid_quantity(
    ): Short
    fun uniffi_ldk_node_checksum_method_offer_issuer(
    ): Short
    fun uniffi_ldk_node_checksum_method_offer_issuer_signing_pubkey(
    ): Short
    fun uniffi_ldk_node_checksum_method_offer_metadata(
    ): Short
    fun uniffi_ldk_node_checksum_method_offer_offer_description(
    ): Short
    fun uniffi_ldk_node_checksum_method_offer_supports_chain(
    ): Short
    fun uniffi_ldk_node_checksum_method_onchainpayment_accelerate_by_cpfp(
    ): Short
    fun uniffi_ldk_node_checksum_method_onchainpayment_bump_fee_by_rbf(
    ): Short
    fun uniffi_ldk_node_checksum_method_onchainpayment_calculate_cpfp_fee_rate(
    ): Short
    fun uniffi_ldk_node_checksum_method_onchainpayment_calculate_total_fee(
    ): Short
    fun uniffi_ldk_node_checksum_method_onchainpayment_list_spendable_outputs(
    ): Short
    fun uniffi_ldk_node_checksum_method_onchainpayment_new_address(
    ): Short
    fun uniffi_ldk_node_checksum_method_onchainpayment_select_utxos_with_algorithm(
    ): Short
    fun uniffi_ldk_node_checksum_method_onchainpayment_send_all_to_address(
    ): Short
    fun uniffi_ldk_node_checksum_method_onchainpayment_send_to_address(
    ): Short
    fun uniffi_ldk_node_checksum_method_refund_absolute_expiry_seconds(
    ): Short
    fun uniffi_ldk_node_checksum_method_refund_amount_msats(
    ): Short
    fun uniffi_ldk_node_checksum_method_refund_chain(
    ): Short
    fun uniffi_ldk_node_checksum_method_refund_is_expired(
    ): Short
    fun uniffi_ldk_node_checksum_method_refund_issuer(
    ): Short
    fun uniffi_ldk_node_checksum_method_refund_payer_metadata(
    ): Short
    fun uniffi_ldk_node_checksum_method_refund_payer_note(
    ): Short
    fun uniffi_ldk_node_checksum_method_refund_payer_signing_pubkey(
    ): Short
    fun uniffi_ldk_node_checksum_method_refund_quantity(
    ): Short
    fun uniffi_ldk_node_checksum_method_refund_refund_description(
    ): Short
    fun uniffi_ldk_node_checksum_method_spontaneouspayment_send(
    ): Short
    fun uniffi_ldk_node_checksum_method_spontaneouspayment_send_probes(
    ): Short
    fun uniffi_ldk_node_checksum_method_spontaneouspayment_send_with_custom_tlvs(
    ): Short
    fun uniffi_ldk_node_checksum_method_spontaneouspayment_send_with_preimage(
    ): Short
    fun uniffi_ldk_node_checksum_method_spontaneouspayment_send_with_preimage_and_custom_tlvs(
    ): Short
    fun uniffi_ldk_node_checksum_method_unifiedqrpayment_receive(
    ): Short
    fun uniffi_ldk_node_checksum_method_unifiedqrpayment_send(
    ): Short
    fun uniffi_ldk_node_checksum_method_vssheaderprovider_get_headers(
    ): Short
    fun uniffi_ldk_node_checksum_constructor_bolt11invoice_from_str(
    ): Short
    fun uniffi_ldk_node_checksum_constructor_bolt12invoice_from_str(
    ): Short
    fun uniffi_ldk_node_checksum_constructor_builder_from_config(
    ): Short
    fun uniffi_ldk_node_checksum_constructor_builder_new(
    ): Short
    fun uniffi_ldk_node_checksum_constructor_feerate_from_sat_per_kwu(
    ): Short
    fun uniffi_ldk_node_checksum_constructor_feerate_from_sat_per_vb_unchecked(
    ): Short
    fun uniffi_ldk_node_checksum_constructor_offer_from_str(
    ): Short
    fun uniffi_ldk_node_checksum_constructor_refund_from_str(
    ): Short
    fun ffi_ldk_node_uniffi_contract_version(
    ): Int
    
}

private fun uniffiCheckContractApiVersion(lib: UniffiLib) {
    // Get the bindings contract version from our ComponentInterface
    val bindings_contract_version = 26
    // Get the scaffolding contract version by calling the into the dylib
    val scaffolding_contract_version = lib.ffi_ldk_node_uniffi_contract_version()
    if (bindings_contract_version != scaffolding_contract_version) {
        throw RuntimeException("UniFFI contract version mismatch: try cleaning and rebuilding your project")
    }
}


private fun uniffiCheckApiChecksums(lib: UniffiLib) {
    if (lib.uniffi_ldk_node_checksum_func_default_config() != 55381.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_func_derive_node_secret_from_mnemonic() != 15067.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_func_generate_entropy_mnemonic() != 48014.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_amount_milli_satoshis() != 50823.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_currency() != 32179.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_expiry_time_seconds() != 23625.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_fallback_addresses() != 55276.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_invoice_description() != 395.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_is_expired() != 15932.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_min_final_cltv_expiry_delta() != 8855.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_network() != 10420.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_payment_hash() != 42571.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_payment_secret() != 26081.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_recover_payee_pub_key() != 18874.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_route_hints() != 63051.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_seconds_since_epoch() != 53979.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_seconds_until_expiry() != 64193.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_signable_hash() != 30910.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11invoice_would_expire() != 30331.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_claim_for_hash() != 52848.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_estimate_routing_fees() != 5123.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_estimate_routing_fees_using_amount() != 46411.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_fail_for_hash() != 24516.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_receive() != 6073.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_receive_for_hash() != 27050.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_receive_variable_amount() != 4893.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_receive_variable_amount_for_hash() != 1402.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_receive_variable_amount_via_jit_channel() != 24506.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_receive_variable_amount_via_jit_channel_for_hash() != 38025.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_receive_via_jit_channel() != 16532.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_receive_via_jit_channel_for_hash() != 1143.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_send() != 12953.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_send_probes() != 19286.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_send_probes_using_amount() != 5976.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt11payment_send_using_amount() != 42793.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_absolute_expiry_seconds() != 28589.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_amount() != 5213.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_amount_msats() != 9297.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_chain() != 3308.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_created_at() != 56866.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_encode() != 13200.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_fallback_addresses() != 7925.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_invoice_description() != 1713.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_is_expired() != 39560.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_issuer() != 65270.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_issuer_signing_pubkey() != 55411.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_metadata() != 37374.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_offer_chains() != 39622.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_payer_note() != 28018.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_payer_signing_pubkey() != 12798.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_payment_hash() != 63778.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_quantity() != 43105.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_relative_expiry() != 14024.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_signable_hash() != 39303.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12invoice_signing_pubkey() != 35202.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12payment_blinded_paths_for_async_recipient() != 14695.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12payment_initiate_refund() != 15019.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12payment_receive() != 59252.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12payment_receive_async() != 23867.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12payment_receive_variable_amount() != 35484.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12payment_request_refund_payment() != 43248.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12payment_send() != 27679.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12payment_send_using_amount() != 33255.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_bolt12payment_set_paths_to_static_invoice_server() != 20921.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_build() != 785.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_build_with_fs_store() != 61304.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_build_with_vss_store() != 2871.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_build_with_vss_store_and_fixed_headers() != 24910.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_build_with_vss_store_and_header_provider() != 9090.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_announcement_addresses() != 39271.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_async_payments_role() != 16463.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_chain_source_bitcoind_rest() != 37382.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_chain_source_bitcoind_rpc() != 2111.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_chain_source_electrum() != 55552.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_chain_source_esplora() != 1781.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_channel_data_migration() != 58453.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_custom_logger() != 51232.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_entropy_bip39_mnemonic() != 827.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_entropy_seed_bytes() != 44799.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_entropy_seed_path() != 64056.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_filesystem_logger() != 10249.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_gossip_source_p2p() != 9279.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_gossip_source_rgs() != 64312.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_liquidity_source_lsps1() != 51527.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_liquidity_source_lsps2() != 14430.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_listening_addresses() != 14051.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_log_facade_logger() != 58410.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_network() != 27539.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_node_alias() != 18342.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_pathfinding_scores_source() != 63501.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_builder_set_storage_dir_path() != 59019.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_feerate_to_sat_per_kwu() != 58911.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_feerate_to_sat_per_vb_ceil() != 58575.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_feerate_to_sat_per_vb_floor() != 59617.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_lsps1liquidity_check_order_status() != 57147.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_lsps1liquidity_request_channel() != 18153.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_logwriter_log() != 3299.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_networkgraph_channel() != 38070.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_networkgraph_list_channels() != 4693.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_networkgraph_list_nodes() != 36715.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_networkgraph_node() != 48925.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_announcement_addresses() != 61426.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_bolt11_payment() != 41402.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_bolt12_payment() != 49254.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_close_channel() != 62479.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_config() != 7511.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_connect() != 34120.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_disconnect() != 43538.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_event_handled() != 38712.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_export_pathfinding_scores() != 62331.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_force_close_channel() != 48831.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_get_address_balance() != 45284.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_get_transaction_details() != 65000.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_list_balances() != 57528.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_list_channels() != 7954.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_list_payments() != 35002.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_list_peers() != 14889.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_listening_addresses() != 2665.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_lsps1_liquidity() != 38201.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_network_graph() != 2695.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_next_event() != 7682.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_next_event_async() != 25426.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_node_alias() != 29526.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_node_id() != 51489.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_onchain_payment() != 6092.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_open_announced_channel() != 36623.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_open_channel() != 40283.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_payment() != 60296.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_remove_payment() != 47952.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_sign_message() != 49319.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_splice_in() != 46431.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_splice_out() != 22115.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_spontaneous_payment() != 37403.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_start() != 58480.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_status() != 55952.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_stop() != 42188.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_sync_wallets() != 32474.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_unified_qr_payment() != 9837.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_update_channel_config() != 37852.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_verify_signature() != 20486.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_node_wait_next_event() != 55101.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_offer_absolute_expiry_seconds() != 22836.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_offer_amount() != 59890.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_offer_chains() != 59522.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_offer_expects_quantity() != 58457.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_offer_id() != 8391.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_offer_is_expired() != 22651.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_offer_is_valid_quantity() != 58469.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_offer_issuer() != 41632.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_offer_issuer_signing_pubkey() != 38162.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_offer_metadata() != 18979.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_offer_offer_description() != 11122.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_offer_supports_chain() != 2135.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_onchainpayment_accelerate_by_cpfp() != 31954.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_onchainpayment_bump_fee_by_rbf() != 53877.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_onchainpayment_calculate_cpfp_fee_rate() != 32879.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_onchainpayment_calculate_total_fee() != 57218.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_onchainpayment_list_spendable_outputs() != 19144.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_onchainpayment_new_address() != 37251.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_onchainpayment_select_utxos_with_algorithm() != 14084.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_onchainpayment_send_all_to_address() != 37748.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_onchainpayment_send_to_address() != 28826.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_refund_absolute_expiry_seconds() != 43722.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_refund_amount_msats() != 26467.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_refund_chain() != 36565.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_refund_is_expired() != 10232.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_refund_issuer() != 40306.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_refund_payer_metadata() != 23501.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_refund_payer_note() != 47799.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_refund_payer_signing_pubkey() != 40880.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_refund_quantity() != 15192.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_refund_refund_description() != 39295.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_spontaneouspayment_send() != 27905.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_spontaneouspayment_send_probes() != 25937.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_spontaneouspayment_send_with_custom_tlvs() != 17876.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_spontaneouspayment_send_with_preimage() != 30854.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_spontaneouspayment_send_with_preimage_and_custom_tlvs() != 12104.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_unifiedqrpayment_receive() != 913.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_unifiedqrpayment_send() != 28285.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_method_vssheaderprovider_get_headers() != 7788.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_constructor_bolt11invoice_from_str() != 349.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_constructor_bolt12invoice_from_str() != 22276.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_constructor_builder_from_config() != 994.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_constructor_builder_new() != 40499.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_constructor_feerate_from_sat_per_kwu() != 50548.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_constructor_feerate_from_sat_per_vb_unchecked() != 41808.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_constructor_offer_from_str() != 37070.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
    if (lib.uniffi_ldk_node_checksum_constructor_refund_from_str() != 64884.toShort()) {
        throw RuntimeException("UniFFI API checksum mismatch: try cleaning and rebuilding your project")
    }
}

// Public interface members begin here.



object FfiConverterUByte: FfiConverter<UByte, Byte> {
    override fun lift(value: Byte): UByte {
        return value.toUByte()
    }

    override fun read(buf: ByteBuffer): UByte {
        return lift(buf.get())
    }

    override fun lower(value: UByte): Byte {
        return value.toByte()
    }

    override fun allocationSize(value: UByte) = 1UL

    override fun write(value: UByte, buf: ByteBuffer) {
        buf.put(value.toByte())
    }
}


object FfiConverterUShort: FfiConverter<UShort, Short> {
    override fun lift(value: Short): UShort {
        return value.toUShort()
    }

    override fun read(buf: ByteBuffer): UShort {
        return lift(buf.getShort())
    }

    override fun lower(value: UShort): Short {
        return value.toShort()
    }

    override fun allocationSize(value: UShort) = 2UL

    override fun write(value: UShort, buf: ByteBuffer) {
        buf.putShort(value.toShort())
    }
}


object FfiConverterUInt: FfiConverter<UInt, Int> {
    override fun lift(value: Int): UInt {
        return value.toUInt()
    }

    override fun read(buf: ByteBuffer): UInt {
        return lift(buf.getInt())
    }

    override fun lower(value: UInt): Int {
        return value.toInt()
    }

    override fun allocationSize(value: UInt) = 4UL

    override fun write(value: UInt, buf: ByteBuffer) {
        buf.putInt(value.toInt())
    }
}


object FfiConverterULong: FfiConverter<ULong, Long> {
    override fun lift(value: Long): ULong {
        return value.toULong()
    }

    override fun read(buf: ByteBuffer): ULong {
        return lift(buf.getLong())
    }

    override fun lower(value: ULong): Long {
        return value.toLong()
    }

    override fun allocationSize(value: ULong) = 8UL

    override fun write(value: ULong, buf: ByteBuffer) {
        buf.putLong(value.toLong())
    }
}


object FfiConverterLong: FfiConverter<Long, Long> {
    override fun lift(value: Long): Long {
        return value
    }

    override fun read(buf: ByteBuffer): Long {
        return buf.getLong()
    }

    override fun lower(value: Long): Long {
        return value
    }

    override fun allocationSize(value: Long) = 8UL

    override fun write(value: Long, buf: ByteBuffer) {
        buf.putLong(value)
    }
}


object FfiConverterBoolean: FfiConverter<Boolean, Byte> {
    override fun lift(value: Byte): Boolean {
        return value.toInt() != 0
    }

    override fun read(buf: ByteBuffer): Boolean {
        return lift(buf.get())
    }

    override fun lower(value: Boolean): Byte {
        return if (value) 1.toByte() else 0.toByte()
    }

    override fun allocationSize(value: Boolean) = 1UL

    override fun write(value: Boolean, buf: ByteBuffer) {
        buf.put(lower(value))
    }
}


fun String.utf8Size(): Int = this.toByteArray(Charsets.UTF_8).size

object FfiConverterString: FfiConverter<String, RustBufferByValue> {
    // Note: we don't inherit from FfiConverterRustBuffer, because we use a
    // special encoding when lowering/lifting.  We can use `RustBuffer.len` to
    // store our length and avoid writing it out to the buffer.
    override fun lift(value: RustBufferByValue): String {
        try {
            require(value.len <= Int.MAX_VALUE) {
        val length = value.len
        "cannot handle RustBuffer longer than Int.MAX_VALUE bytes: length is $length"
    }
            val byteArr =  value.asByteBuffer()!!.get(value.len.toInt())
            return byteArr.decodeToString()
        } finally {
            RustBufferHelper.free(value)
        }
    }

    override fun read(buf: ByteBuffer): String {
        val len = buf.getInt()
        val byteArr = buf.get(len)
        return byteArr.decodeToString()
    }

    override fun lower(value: String): RustBufferByValue {
        return RustBufferHelper.allocValue(value.utf8Size().toULong()).apply {
            asByteBuffer()!!.writeUtf8(value)
        }
    }

    // We aren't sure exactly how many bytes our string will be once it's UTF-8
    // encoded.  Allocate 3 bytes per UTF-16 code unit which will always be
    // enough.
    override fun allocationSize(value: String): ULong {
        val sizeForLength = 4UL
        val sizeForString = value.length.toULong() * 3UL
        return sizeForLength + sizeForString
    }

    override fun write(value: String, buf: ByteBuffer) {
        buf.putInt(value.utf8Size().toInt())
        buf.writeUtf8(value)
    }
}


object FfiConverterByteArray: FfiConverterRustBuffer<ByteArray> {
    override fun read(buf: ByteBuffer): ByteArray {
        val len = buf.getInt()
        val byteArr = buf.get(len)
        return byteArr
    }
    override fun allocationSize(value: ByteArray): ULong {
        return 4UL + value.size.toULong()
    }
    override fun write(value: ByteArray, buf: ByteBuffer) {
        buf.putInt(value.size)
        buf.put(value)
    }
}



open class Bolt11Invoice: Disposable, Bolt11InvoiceInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_bolt11invoice(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_bolt11invoice(pointer!!, status)
        }!!
    }

    
    override fun `amountMilliSatoshis`(): kotlin.ULong? {
        return FfiConverterOptionalULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_amount_milli_satoshis(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `currency`(): Currency {
        return FfiConverterTypeCurrency.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_currency(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `expiryTimeSeconds`(): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_expiry_time_seconds(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `fallbackAddresses`(): List<Address> {
        return FfiConverterSequenceTypeAddress.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_fallback_addresses(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `invoiceDescription`(): Bolt11InvoiceDescription {
        return FfiConverterTypeBolt11InvoiceDescription.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_invoice_description(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `isExpired`(): kotlin.Boolean {
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_is_expired(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `minFinalCltvExpiryDelta`(): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_min_final_cltv_expiry_delta(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `network`(): Network {
        return FfiConverterTypeNetwork.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_network(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `paymentHash`(): PaymentHash {
        return FfiConverterTypePaymentHash.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_payment_hash(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `paymentSecret`(): PaymentSecret {
        return FfiConverterTypePaymentSecret.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_payment_secret(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `recoverPayeePubKey`(): PublicKey {
        return FfiConverterTypePublicKey.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_recover_payee_pub_key(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `routeHints`(): List<List<RouteHintHop>> {
        return FfiConverterSequenceSequenceTypeRouteHintHop.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_route_hints(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `secondsSinceEpoch`(): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_seconds_since_epoch(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `secondsUntilExpiry`(): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_seconds_until_expiry(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `signableHash`(): List<kotlin.UByte> {
        return FfiConverterSequenceUByte.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_signable_hash(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `wouldExpire`(`atTimeSeconds`: kotlin.ULong): kotlin.Boolean {
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_would_expire(
                    it,
                    FfiConverterULong.lower(`atTimeSeconds`),
                    uniffiRustCallStatus,
                )
            }
        })
    }


    
    
    override fun toString(): String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_uniffi_trait_display(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }
    
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is Bolt11Invoice) return false
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11invoice_uniffi_trait_eq_eq(
                    it,
                    FfiConverterTypeBolt11Invoice.lower(`other`),
                    uniffiRustCallStatus,
                )
            }
        })
    }
    

    
    companion object {
        
        @Throws(NodeException::class)
        fun `fromStr`(`invoiceStr`: kotlin.String): Bolt11Invoice {
            return FfiConverterTypeBolt11Invoice.lift(uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_constructor_bolt11invoice_from_str(
                    FfiConverterString.lower(`invoiceStr`),
                    uniffiRustCallStatus,
                )
            }!!)
        }

        
    }
    
}





object FfiConverterTypeBolt11Invoice: FfiConverter<Bolt11Invoice, Pointer> {

    override fun lower(value: Bolt11Invoice): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): Bolt11Invoice {
        return Bolt11Invoice(value)
    }

    override fun read(buf: ByteBuffer): Bolt11Invoice {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: Bolt11Invoice) = 8UL

    override fun write(value: Bolt11Invoice, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}
// The cleaner interface for Object finalization code to run.
// This is the entry point to any implementation that we're using.
//
// The cleaner registers disposables and returns cleanables, so now we are
// defining a `UniffiCleaner` with a `UniffiClenaer.Cleanable` to abstract the
// different implementations available at compile time.
interface UniffiCleaner {
    interface Cleanable {
        fun clean()
    }

    fun register(resource: Any, disposable: Disposable): UniffiCleaner.Cleanable

    companion object
}
// The fallback Jna cleaner, which is available for both Android, and the JVM.
private class UniffiJnaCleaner : UniffiCleaner {
    private val cleaner = com.sun.jna.internal.Cleaner.getCleaner()

    override fun register(resource: Any, disposable: Disposable): UniffiCleaner.Cleanable =
        UniffiJnaCleanable(cleaner.register(resource, UniffiCleanerAction(disposable)))
}

private class UniffiJnaCleanable(
    private val cleanable: com.sun.jna.internal.Cleaner.Cleanable,
) : UniffiCleaner.Cleanable {
    override fun clean() = cleanable.clean()
}

private class UniffiCleanerAction(private val disposable: Disposable): Runnable {
    override fun run() {
        disposable.destroy()
    }
}

// The SystemCleaner, available from API Level 33.
// Some API Level 33 OSes do not support using it, so we require API Level 34.
@RequiresApi(Build.VERSION_CODES.UPSIDE_DOWN_CAKE)
private class AndroidSystemCleaner : UniffiCleaner {
    private val cleaner = android.system.SystemCleaner.cleaner()

    override fun register(resource: Any, disposable: Disposable): UniffiCleaner.Cleanable =
        AndroidSystemCleanable(cleaner.register(resource, UniffiCleanerAction(disposable)))
}

@RequiresApi(Build.VERSION_CODES.UPSIDE_DOWN_CAKE)
private class AndroidSystemCleanable(
    private val cleanable: java.lang.ref.Cleaner.Cleanable,
) : UniffiCleaner.Cleanable {
    override fun clean() = cleanable.clean()
}

private fun UniffiCleaner.Companion.create(): UniffiCleaner {
    if (Build.VERSION.SDK_INT >= Build.VERSION_CODES.UPSIDE_DOWN_CAKE) {
        try {
            return AndroidSystemCleaner()
        } catch (_: IllegalAccessError) {
            // (For Compose preview) Fallback to UniffiJnaCleaner if AndroidSystemCleaner is
            // unavailable, even for API level 34 or higher.
        }
    }
    return UniffiJnaCleaner()
}



open class Bolt11Payment: Disposable, Bolt11PaymentInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_bolt11payment(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_bolt11payment(pointer!!, status)
        }!!
    }

    
    @Throws(NodeException::class)
    override fun `claimForHash`(`paymentHash`: PaymentHash, `claimableAmountMsat`: kotlin.ULong, `preimage`: PaymentPreimage) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_claim_for_hash(
                    it,
                    FfiConverterTypePaymentHash.lower(`paymentHash`),
                    FfiConverterULong.lower(`claimableAmountMsat`),
                    FfiConverterTypePaymentPreimage.lower(`preimage`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(NodeException::class)
    override fun `estimateRoutingFees`(`invoice`: Bolt11Invoice): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_estimate_routing_fees(
                    it,
                    FfiConverterTypeBolt11Invoice.lower(`invoice`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `estimateRoutingFeesUsingAmount`(`invoice`: Bolt11Invoice, `amountMsat`: kotlin.ULong): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_estimate_routing_fees_using_amount(
                    it,
                    FfiConverterTypeBolt11Invoice.lower(`invoice`),
                    FfiConverterULong.lower(`amountMsat`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `failForHash`(`paymentHash`: PaymentHash) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_fail_for_hash(
                    it,
                    FfiConverterTypePaymentHash.lower(`paymentHash`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(NodeException::class)
    override fun `receive`(`amountMsat`: kotlin.ULong, `description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt): Bolt11Invoice {
        return FfiConverterTypeBolt11Invoice.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_receive(
                    it,
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterTypeBolt11InvoiceDescription.lower(`description`),
                    FfiConverterUInt.lower(`expirySecs`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `receiveForHash`(`amountMsat`: kotlin.ULong, `description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `paymentHash`: PaymentHash): Bolt11Invoice {
        return FfiConverterTypeBolt11Invoice.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_receive_for_hash(
                    it,
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterTypeBolt11InvoiceDescription.lower(`description`),
                    FfiConverterUInt.lower(`expirySecs`),
                    FfiConverterTypePaymentHash.lower(`paymentHash`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `receiveVariableAmount`(`description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt): Bolt11Invoice {
        return FfiConverterTypeBolt11Invoice.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_receive_variable_amount(
                    it,
                    FfiConverterTypeBolt11InvoiceDescription.lower(`description`),
                    FfiConverterUInt.lower(`expirySecs`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `receiveVariableAmountForHash`(`description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `paymentHash`: PaymentHash): Bolt11Invoice {
        return FfiConverterTypeBolt11Invoice.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_receive_variable_amount_for_hash(
                    it,
                    FfiConverterTypeBolt11InvoiceDescription.lower(`description`),
                    FfiConverterUInt.lower(`expirySecs`),
                    FfiConverterTypePaymentHash.lower(`paymentHash`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `receiveVariableAmountViaJitChannel`(`description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `maxProportionalLspFeeLimitPpmMsat`: kotlin.ULong?): Bolt11Invoice {
        return FfiConverterTypeBolt11Invoice.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_receive_variable_amount_via_jit_channel(
                    it,
                    FfiConverterTypeBolt11InvoiceDescription.lower(`description`),
                    FfiConverterUInt.lower(`expirySecs`),
                    FfiConverterOptionalULong.lower(`maxProportionalLspFeeLimitPpmMsat`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `receiveVariableAmountViaJitChannelForHash`(`description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `maxProportionalLspFeeLimitPpmMsat`: kotlin.ULong?, `paymentHash`: PaymentHash): Bolt11Invoice {
        return FfiConverterTypeBolt11Invoice.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_receive_variable_amount_via_jit_channel_for_hash(
                    it,
                    FfiConverterTypeBolt11InvoiceDescription.lower(`description`),
                    FfiConverterUInt.lower(`expirySecs`),
                    FfiConverterOptionalULong.lower(`maxProportionalLspFeeLimitPpmMsat`),
                    FfiConverterTypePaymentHash.lower(`paymentHash`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `receiveViaJitChannel`(`amountMsat`: kotlin.ULong, `description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `maxLspFeeLimitMsat`: kotlin.ULong?): Bolt11Invoice {
        return FfiConverterTypeBolt11Invoice.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_receive_via_jit_channel(
                    it,
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterTypeBolt11InvoiceDescription.lower(`description`),
                    FfiConverterUInt.lower(`expirySecs`),
                    FfiConverterOptionalULong.lower(`maxLspFeeLimitMsat`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `receiveViaJitChannelForHash`(`amountMsat`: kotlin.ULong, `description`: Bolt11InvoiceDescription, `expirySecs`: kotlin.UInt, `maxLspFeeLimitMsat`: kotlin.ULong?, `paymentHash`: PaymentHash): Bolt11Invoice {
        return FfiConverterTypeBolt11Invoice.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_receive_via_jit_channel_for_hash(
                    it,
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterTypeBolt11InvoiceDescription.lower(`description`),
                    FfiConverterUInt.lower(`expirySecs`),
                    FfiConverterOptionalULong.lower(`maxLspFeeLimitMsat`),
                    FfiConverterTypePaymentHash.lower(`paymentHash`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `send`(`invoice`: Bolt11Invoice, `routeParameters`: RouteParametersConfig?): PaymentId {
        return FfiConverterTypePaymentId.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_send(
                    it,
                    FfiConverterTypeBolt11Invoice.lower(`invoice`),
                    FfiConverterOptionalTypeRouteParametersConfig.lower(`routeParameters`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `sendProbes`(`invoice`: Bolt11Invoice, `routeParameters`: RouteParametersConfig?) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_send_probes(
                    it,
                    FfiConverterTypeBolt11Invoice.lower(`invoice`),
                    FfiConverterOptionalTypeRouteParametersConfig.lower(`routeParameters`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(NodeException::class)
    override fun `sendProbesUsingAmount`(`invoice`: Bolt11Invoice, `amountMsat`: kotlin.ULong, `routeParameters`: RouteParametersConfig?) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_send_probes_using_amount(
                    it,
                    FfiConverterTypeBolt11Invoice.lower(`invoice`),
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterOptionalTypeRouteParametersConfig.lower(`routeParameters`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(NodeException::class)
    override fun `sendUsingAmount`(`invoice`: Bolt11Invoice, `amountMsat`: kotlin.ULong, `routeParameters`: RouteParametersConfig?): PaymentId {
        return FfiConverterTypePaymentId.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt11payment_send_using_amount(
                    it,
                    FfiConverterTypeBolt11Invoice.lower(`invoice`),
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterOptionalTypeRouteParametersConfig.lower(`routeParameters`),
                    uniffiRustCallStatus,
                )
            }
        })
    }


    
    

    
    
    companion object
    
}





object FfiConverterTypeBolt11Payment: FfiConverter<Bolt11Payment, Pointer> {

    override fun lower(value: Bolt11Payment): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): Bolt11Payment {
        return Bolt11Payment(value)
    }

    override fun read(buf: ByteBuffer): Bolt11Payment {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: Bolt11Payment) = 8UL

    override fun write(value: Bolt11Payment, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class Bolt12Invoice: Disposable, Bolt12InvoiceInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_bolt12invoice(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_bolt12invoice(pointer!!, status)
        }!!
    }

    
    override fun `absoluteExpirySeconds`(): kotlin.ULong? {
        return FfiConverterOptionalULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_absolute_expiry_seconds(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `amount`(): OfferAmount? {
        return FfiConverterOptionalTypeOfferAmount.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_amount(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `amountMsats`(): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_amount_msats(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `chain`(): List<kotlin.UByte> {
        return FfiConverterSequenceUByte.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_chain(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `createdAt`(): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_created_at(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `encode`(): List<kotlin.UByte> {
        return FfiConverterSequenceUByte.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_encode(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `fallbackAddresses`(): List<Address> {
        return FfiConverterSequenceTypeAddress.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_fallback_addresses(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `invoiceDescription`(): kotlin.String? {
        return FfiConverterOptionalString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_invoice_description(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `isExpired`(): kotlin.Boolean {
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_is_expired(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `issuer`(): kotlin.String? {
        return FfiConverterOptionalString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_issuer(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `issuerSigningPubkey`(): PublicKey? {
        return FfiConverterOptionalTypePublicKey.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_issuer_signing_pubkey(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `metadata`(): List<kotlin.UByte>? {
        return FfiConverterOptionalSequenceUByte.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_metadata(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `offerChains`(): List<List<kotlin.UByte>>? {
        return FfiConverterOptionalSequenceSequenceUByte.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_offer_chains(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `payerNote`(): kotlin.String? {
        return FfiConverterOptionalString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_payer_note(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `payerSigningPubkey`(): PublicKey {
        return FfiConverterTypePublicKey.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_payer_signing_pubkey(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `paymentHash`(): PaymentHash {
        return FfiConverterTypePaymentHash.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_payment_hash(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `quantity`(): kotlin.ULong? {
        return FfiConverterOptionalULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_quantity(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `relativeExpiry`(): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_relative_expiry(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `signableHash`(): List<kotlin.UByte> {
        return FfiConverterSequenceUByte.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_signable_hash(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `signingPubkey`(): PublicKey {
        return FfiConverterTypePublicKey.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12invoice_signing_pubkey(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }


    
    

    
    companion object {
        
        @Throws(NodeException::class)
        fun `fromStr`(`invoiceStr`: kotlin.String): Bolt12Invoice {
            return FfiConverterTypeBolt12Invoice.lift(uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_constructor_bolt12invoice_from_str(
                    FfiConverterString.lower(`invoiceStr`),
                    uniffiRustCallStatus,
                )
            }!!)
        }

        
    }
    
}





object FfiConverterTypeBolt12Invoice: FfiConverter<Bolt12Invoice, Pointer> {

    override fun lower(value: Bolt12Invoice): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): Bolt12Invoice {
        return Bolt12Invoice(value)
    }

    override fun read(buf: ByteBuffer): Bolt12Invoice {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: Bolt12Invoice) = 8UL

    override fun write(value: Bolt12Invoice, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class Bolt12Payment: Disposable, Bolt12PaymentInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_bolt12payment(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_bolt12payment(pointer!!, status)
        }!!
    }

    
    @Throws(NodeException::class)
    override fun `blindedPathsForAsyncRecipient`(`recipientId`: kotlin.ByteArray): kotlin.ByteArray {
        return FfiConverterByteArray.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12payment_blinded_paths_for_async_recipient(
                    it,
                    FfiConverterByteArray.lower(`recipientId`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `initiateRefund`(`amountMsat`: kotlin.ULong, `expirySecs`: kotlin.UInt, `quantity`: kotlin.ULong?, `payerNote`: kotlin.String?, `routeParameters`: RouteParametersConfig?): Refund {
        return FfiConverterTypeRefund.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12payment_initiate_refund(
                    it,
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterUInt.lower(`expirySecs`),
                    FfiConverterOptionalULong.lower(`quantity`),
                    FfiConverterOptionalString.lower(`payerNote`),
                    FfiConverterOptionalTypeRouteParametersConfig.lower(`routeParameters`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `receive`(`amountMsat`: kotlin.ULong, `description`: kotlin.String, `expirySecs`: kotlin.UInt?, `quantity`: kotlin.ULong?): Offer {
        return FfiConverterTypeOffer.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12payment_receive(
                    it,
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterString.lower(`description`),
                    FfiConverterOptionalUInt.lower(`expirySecs`),
                    FfiConverterOptionalULong.lower(`quantity`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `receiveAsync`(): Offer {
        return FfiConverterTypeOffer.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12payment_receive_async(
                    it,
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `receiveVariableAmount`(`description`: kotlin.String, `expirySecs`: kotlin.UInt?): Offer {
        return FfiConverterTypeOffer.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12payment_receive_variable_amount(
                    it,
                    FfiConverterString.lower(`description`),
                    FfiConverterOptionalUInt.lower(`expirySecs`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `requestRefundPayment`(`refund`: Refund): Bolt12Invoice {
        return FfiConverterTypeBolt12Invoice.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12payment_request_refund_payment(
                    it,
                    FfiConverterTypeRefund.lower(`refund`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `send`(`offer`: Offer, `quantity`: kotlin.ULong?, `payerNote`: kotlin.String?, `routeParameters`: RouteParametersConfig?): PaymentId {
        return FfiConverterTypePaymentId.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12payment_send(
                    it,
                    FfiConverterTypeOffer.lower(`offer`),
                    FfiConverterOptionalULong.lower(`quantity`),
                    FfiConverterOptionalString.lower(`payerNote`),
                    FfiConverterOptionalTypeRouteParametersConfig.lower(`routeParameters`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `sendUsingAmount`(`offer`: Offer, `amountMsat`: kotlin.ULong, `quantity`: kotlin.ULong?, `payerNote`: kotlin.String?, `routeParameters`: RouteParametersConfig?): PaymentId {
        return FfiConverterTypePaymentId.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12payment_send_using_amount(
                    it,
                    FfiConverterTypeOffer.lower(`offer`),
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterOptionalULong.lower(`quantity`),
                    FfiConverterOptionalString.lower(`payerNote`),
                    FfiConverterOptionalTypeRouteParametersConfig.lower(`routeParameters`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `setPathsToStaticInvoiceServer`(`paths`: kotlin.ByteArray) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_bolt12payment_set_paths_to_static_invoice_server(
                    it,
                    FfiConverterByteArray.lower(`paths`),
                    uniffiRustCallStatus,
                )
            }
        }
    }


    
    

    
    
    companion object
    
}





object FfiConverterTypeBolt12Payment: FfiConverter<Bolt12Payment, Pointer> {

    override fun lower(value: Bolt12Payment): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): Bolt12Payment {
        return Bolt12Payment(value)
    }

    override fun read(buf: ByteBuffer): Bolt12Payment {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: Bolt12Payment) = 8UL

    override fun write(value: Bolt12Payment, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class Builder: Disposable, BuilderInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    constructor() : this(
        uniffiRustCall { uniffiRustCallStatus ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_constructor_builder_new(
                uniffiRustCallStatus,
            )
        }!!
    )

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_builder(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_builder(pointer!!, status)
        }!!
    }

    
    @Throws(BuildException::class)
    override fun `build`(): Node {
        return FfiConverterTypeNode.lift(callWithPointer {
            uniffiRustCallWithError(BuildExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_build(
                    it,
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(BuildException::class)
    override fun `buildWithFsStore`(): Node {
        return FfiConverterTypeNode.lift(callWithPointer {
            uniffiRustCallWithError(BuildExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_build_with_fs_store(
                    it,
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(BuildException::class)
    override fun `buildWithVssStore`(`vssUrl`: kotlin.String, `storeId`: kotlin.String, `lnurlAuthServerUrl`: kotlin.String, `fixedHeaders`: Map<kotlin.String, kotlin.String>): Node {
        return FfiConverterTypeNode.lift(callWithPointer {
            uniffiRustCallWithError(BuildExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_build_with_vss_store(
                    it,
                    FfiConverterString.lower(`vssUrl`),
                    FfiConverterString.lower(`storeId`),
                    FfiConverterString.lower(`lnurlAuthServerUrl`),
                    FfiConverterMapStringString.lower(`fixedHeaders`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(BuildException::class)
    override fun `buildWithVssStoreAndFixedHeaders`(`vssUrl`: kotlin.String, `storeId`: kotlin.String, `fixedHeaders`: Map<kotlin.String, kotlin.String>): Node {
        return FfiConverterTypeNode.lift(callWithPointer {
            uniffiRustCallWithError(BuildExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_build_with_vss_store_and_fixed_headers(
                    it,
                    FfiConverterString.lower(`vssUrl`),
                    FfiConverterString.lower(`storeId`),
                    FfiConverterMapStringString.lower(`fixedHeaders`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(BuildException::class)
    override fun `buildWithVssStoreAndHeaderProvider`(`vssUrl`: kotlin.String, `storeId`: kotlin.String, `headerProvider`: VssHeaderProvider): Node {
        return FfiConverterTypeNode.lift(callWithPointer {
            uniffiRustCallWithError(BuildExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_build_with_vss_store_and_header_provider(
                    it,
                    FfiConverterString.lower(`vssUrl`),
                    FfiConverterString.lower(`storeId`),
                    FfiConverterTypeVssHeaderProvider.lower(`headerProvider`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(BuildException::class)
    override fun `setAnnouncementAddresses`(`announcementAddresses`: List<SocketAddress>) {
        callWithPointer {
            uniffiRustCallWithError(BuildExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_announcement_addresses(
                    it,
                    FfiConverterSequenceTypeSocketAddress.lower(`announcementAddresses`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(BuildException::class)
    override fun `setAsyncPaymentsRole`(`role`: AsyncPaymentsRole?) {
        callWithPointer {
            uniffiRustCallWithError(BuildExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_async_payments_role(
                    it,
                    FfiConverterOptionalTypeAsyncPaymentsRole.lower(`role`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setChainSourceBitcoindRest`(`restHost`: kotlin.String, `restPort`: kotlin.UShort, `rpcHost`: kotlin.String, `rpcPort`: kotlin.UShort, `rpcUser`: kotlin.String, `rpcPassword`: kotlin.String) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_chain_source_bitcoind_rest(
                    it,
                    FfiConverterString.lower(`restHost`),
                    FfiConverterUShort.lower(`restPort`),
                    FfiConverterString.lower(`rpcHost`),
                    FfiConverterUShort.lower(`rpcPort`),
                    FfiConverterString.lower(`rpcUser`),
                    FfiConverterString.lower(`rpcPassword`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setChainSourceBitcoindRpc`(`rpcHost`: kotlin.String, `rpcPort`: kotlin.UShort, `rpcUser`: kotlin.String, `rpcPassword`: kotlin.String) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_chain_source_bitcoind_rpc(
                    it,
                    FfiConverterString.lower(`rpcHost`),
                    FfiConverterUShort.lower(`rpcPort`),
                    FfiConverterString.lower(`rpcUser`),
                    FfiConverterString.lower(`rpcPassword`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setChainSourceElectrum`(`serverUrl`: kotlin.String, `config`: ElectrumSyncConfig?) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_chain_source_electrum(
                    it,
                    FfiConverterString.lower(`serverUrl`),
                    FfiConverterOptionalTypeElectrumSyncConfig.lower(`config`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setChainSourceEsplora`(`serverUrl`: kotlin.String, `config`: EsploraSyncConfig?) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_chain_source_esplora(
                    it,
                    FfiConverterString.lower(`serverUrl`),
                    FfiConverterOptionalTypeEsploraSyncConfig.lower(`config`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setChannelDataMigration`(`migration`: ChannelDataMigration) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_channel_data_migration(
                    it,
                    FfiConverterTypeChannelDataMigration.lower(`migration`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setCustomLogger`(`logWriter`: LogWriter) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_custom_logger(
                    it,
                    FfiConverterTypeLogWriter.lower(`logWriter`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setEntropyBip39Mnemonic`(`mnemonic`: Mnemonic, `passphrase`: kotlin.String?) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_entropy_bip39_mnemonic(
                    it,
                    FfiConverterTypeMnemonic.lower(`mnemonic`),
                    FfiConverterOptionalString.lower(`passphrase`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(BuildException::class)
    override fun `setEntropySeedBytes`(`seedBytes`: List<kotlin.UByte>) {
        callWithPointer {
            uniffiRustCallWithError(BuildExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_entropy_seed_bytes(
                    it,
                    FfiConverterSequenceUByte.lower(`seedBytes`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setEntropySeedPath`(`seedPath`: kotlin.String) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_entropy_seed_path(
                    it,
                    FfiConverterString.lower(`seedPath`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setFilesystemLogger`(`logFilePath`: kotlin.String?, `maxLogLevel`: LogLevel?) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_filesystem_logger(
                    it,
                    FfiConverterOptionalString.lower(`logFilePath`),
                    FfiConverterOptionalTypeLogLevel.lower(`maxLogLevel`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setGossipSourceP2p`() {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_gossip_source_p2p(
                    it,
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setGossipSourceRgs`(`rgsServerUrl`: kotlin.String) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_gossip_source_rgs(
                    it,
                    FfiConverterString.lower(`rgsServerUrl`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setLiquiditySourceLsps1`(`nodeId`: PublicKey, `address`: SocketAddress, `token`: kotlin.String?) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_liquidity_source_lsps1(
                    it,
                    FfiConverterTypePublicKey.lower(`nodeId`),
                    FfiConverterTypeSocketAddress.lower(`address`),
                    FfiConverterOptionalString.lower(`token`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setLiquiditySourceLsps2`(`nodeId`: PublicKey, `address`: SocketAddress, `token`: kotlin.String?) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_liquidity_source_lsps2(
                    it,
                    FfiConverterTypePublicKey.lower(`nodeId`),
                    FfiConverterTypeSocketAddress.lower(`address`),
                    FfiConverterOptionalString.lower(`token`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(BuildException::class)
    override fun `setListeningAddresses`(`listeningAddresses`: List<SocketAddress>) {
        callWithPointer {
            uniffiRustCallWithError(BuildExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_listening_addresses(
                    it,
                    FfiConverterSequenceTypeSocketAddress.lower(`listeningAddresses`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setLogFacadeLogger`() {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_log_facade_logger(
                    it,
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setNetwork`(`network`: Network) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_network(
                    it,
                    FfiConverterTypeNetwork.lower(`network`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(BuildException::class)
    override fun `setNodeAlias`(`nodeAlias`: kotlin.String) {
        callWithPointer {
            uniffiRustCallWithError(BuildExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_node_alias(
                    it,
                    FfiConverterString.lower(`nodeAlias`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setPathfindingScoresSource`(`url`: kotlin.String) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_pathfinding_scores_source(
                    it,
                    FfiConverterString.lower(`url`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `setStorageDirPath`(`storageDirPath`: kotlin.String) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_builder_set_storage_dir_path(
                    it,
                    FfiConverterString.lower(`storageDirPath`),
                    uniffiRustCallStatus,
                )
            }
        }
    }


    
    

    
    companion object {
        
        fun `fromConfig`(`config`: Config): Builder {
            return FfiConverterTypeBuilder.lift(uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_constructor_builder_from_config(
                    FfiConverterTypeConfig.lower(`config`),
                    uniffiRustCallStatus,
                )
            }!!)
        }

        
    }
    
}





object FfiConverterTypeBuilder: FfiConverter<Builder, Pointer> {

    override fun lower(value: Builder): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): Builder {
        return Builder(value)
    }

    override fun read(buf: ByteBuffer): Builder {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: Builder) = 8UL

    override fun write(value: Builder, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class FeeRate: Disposable, FeeRateInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_feerate(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_feerate(pointer!!, status)
        }!!
    }

    
    override fun `toSatPerKwu`(): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_feerate_to_sat_per_kwu(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `toSatPerVbCeil`(): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_feerate_to_sat_per_vb_ceil(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `toSatPerVbFloor`(): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_feerate_to_sat_per_vb_floor(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }


    
    

    
    companion object {
        
        fun `fromSatPerKwu`(`satKwu`: kotlin.ULong): FeeRate {
            return FfiConverterTypeFeeRate.lift(uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_constructor_feerate_from_sat_per_kwu(
                    FfiConverterULong.lower(`satKwu`),
                    uniffiRustCallStatus,
                )
            }!!)
        }

        
        fun `fromSatPerVbUnchecked`(`satVb`: kotlin.ULong): FeeRate {
            return FfiConverterTypeFeeRate.lift(uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_constructor_feerate_from_sat_per_vb_unchecked(
                    FfiConverterULong.lower(`satVb`),
                    uniffiRustCallStatus,
                )
            }!!)
        }

        
    }
    
}





object FfiConverterTypeFeeRate: FfiConverter<FeeRate, Pointer> {

    override fun lower(value: FeeRate): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): FeeRate {
        return FeeRate(value)
    }

    override fun read(buf: ByteBuffer): FeeRate {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: FeeRate) = 8UL

    override fun write(value: FeeRate, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class Lsps1Liquidity: Disposable, Lsps1LiquidityInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_lsps1liquidity(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_lsps1liquidity(pointer!!, status)
        }!!
    }

    
    @Throws(NodeException::class)
    override fun `checkOrderStatus`(`orderId`: Lsps1OrderId): Lsps1OrderStatus {
        return FfiConverterTypeLSPS1OrderStatus.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_lsps1liquidity_check_order_status(
                    it,
                    FfiConverterTypeLSPS1OrderId.lower(`orderId`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `requestChannel`(`lspBalanceSat`: kotlin.ULong, `clientBalanceSat`: kotlin.ULong, `channelExpiryBlocks`: kotlin.UInt, `announceChannel`: kotlin.Boolean): Lsps1OrderStatus {
        return FfiConverterTypeLSPS1OrderStatus.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_lsps1liquidity_request_channel(
                    it,
                    FfiConverterULong.lower(`lspBalanceSat`),
                    FfiConverterULong.lower(`clientBalanceSat`),
                    FfiConverterUInt.lower(`channelExpiryBlocks`),
                    FfiConverterBoolean.lower(`announceChannel`),
                    uniffiRustCallStatus,
                )
            }
        })
    }


    
    

    
    
    companion object
    
}





object FfiConverterTypeLSPS1Liquidity: FfiConverter<Lsps1Liquidity, Pointer> {

    override fun lower(value: Lsps1Liquidity): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): Lsps1Liquidity {
        return Lsps1Liquidity(value)
    }

    override fun read(buf: ByteBuffer): Lsps1Liquidity {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: Lsps1Liquidity) = 8UL

    override fun write(value: Lsps1Liquidity, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class LogWriterImpl: Disposable, LogWriter {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_logwriter(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_logwriter(pointer!!, status)
        }!!
    }

    
    override fun `log`(`record`: LogRecord) {
        callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_logwriter_log(
                    it,
                    FfiConverterTypeLogRecord.lower(`record`),
                    uniffiRustCallStatus,
                )
            }
        }
    }


    
    

    
    
    companion object
    
}





object FfiConverterTypeLogWriter: FfiConverter<LogWriter, Pointer> {
    internal val handleMap = UniffiHandleMap<LogWriter>()

    override fun lower(value: LogWriter): Pointer {
        return handleMap.insert(value).toPointer()
    }

    override fun lift(value: Pointer): LogWriter {
        return LogWriterImpl(value)
    }

    override fun read(buf: ByteBuffer): LogWriter {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: LogWriter) = 8UL

    override fun write(value: LogWriter, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}

internal const val IDX_CALLBACK_FREE = 0
// Callback return codes
internal const val UNIFFI_CALLBACK_SUCCESS = 0
internal const val UNIFFI_CALLBACK_ERROR = 1
internal const val UNIFFI_CALLBACK_UNEXPECTED_ERROR = 2

abstract class FfiConverterCallbackInterface<CallbackInterface: Any>: FfiConverter<CallbackInterface, Long> {
    internal val handleMap = UniffiHandleMap<CallbackInterface>()

    internal fun drop(handle: Long) {
        handleMap.remove(handle)
    }

    override fun lift(value: Long): CallbackInterface {
        return handleMap.get(value)
    }

    override fun read(buf: ByteBuffer) = lift(buf.getLong())

    override fun lower(value: CallbackInterface) = handleMap.insert(value)

    override fun allocationSize(value: CallbackInterface) = 8UL

    override fun write(value: CallbackInterface, buf: ByteBuffer) {
        buf.putLong(lower(value))
    }
}

// Put the implementation in an object so we don't pollute the top-level namespace
internal object uniffiCallbackInterfaceLogWriter {
    internal object `log`: UniffiCallbackInterfaceLogWriterMethod0 {
        override fun callback (
            `uniffiHandle`: Long,
            `record`: RustBufferByValue,
            `uniffiOutReturn`: Pointer,
            uniffiCallStatus: UniffiRustCallStatus,
        ) {
            val uniffiObj = FfiConverterTypeLogWriter.handleMap.get(uniffiHandle)
            val makeCall = { ->
                uniffiObj.`log`(
                    FfiConverterTypeLogRecord.lift(`record`),
                )
            }
            val writeReturn = { _: Unit ->
                @Suppress("UNUSED_EXPRESSION")
                uniffiOutReturn
                Unit
            }
            uniffiTraitInterfaceCall(uniffiCallStatus, makeCall, writeReturn)
        }
    }
    internal object uniffiFree: UniffiCallbackInterfaceFree {
        override fun callback(handle: Long) {
            FfiConverterTypeLogWriter.handleMap.remove(handle)
        }
    }

    internal val vtable = UniffiVTableCallbackInterfaceLogWriter(
        `log`,
        uniffiFree,
    )

    internal fun register(lib: UniffiLib) {
        lib.uniffi_ldk_node_fn_init_callback_vtable_logwriter(vtable)
    }
}



open class NetworkGraph: Disposable, NetworkGraphInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_networkgraph(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_networkgraph(pointer!!, status)
        }!!
    }

    
    override fun `channel`(`shortChannelId`: kotlin.ULong): ChannelInfo? {
        return FfiConverterOptionalTypeChannelInfo.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_networkgraph_channel(
                    it,
                    FfiConverterULong.lower(`shortChannelId`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `listChannels`(): List<kotlin.ULong> {
        return FfiConverterSequenceULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_networkgraph_list_channels(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `listNodes`(): List<NodeId> {
        return FfiConverterSequenceTypeNodeId.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_networkgraph_list_nodes(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `node`(`nodeId`: NodeId): NodeInfo? {
        return FfiConverterOptionalTypeNodeInfo.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_networkgraph_node(
                    it,
                    FfiConverterTypeNodeId.lower(`nodeId`),
                    uniffiRustCallStatus,
                )
            }
        })
    }


    
    

    
    
    companion object
    
}





object FfiConverterTypeNetworkGraph: FfiConverter<NetworkGraph, Pointer> {

    override fun lower(value: NetworkGraph): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): NetworkGraph {
        return NetworkGraph(value)
    }

    override fun read(buf: ByteBuffer): NetworkGraph {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: NetworkGraph) = 8UL

    override fun write(value: NetworkGraph, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class Node: Disposable, NodeInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_node(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_node(pointer!!, status)
        }!!
    }

    
    override fun `announcementAddresses`(): List<SocketAddress>? {
        return FfiConverterOptionalSequenceTypeSocketAddress.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_announcement_addresses(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `bolt11Payment`(): Bolt11Payment {
        return FfiConverterTypeBolt11Payment.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_bolt11_payment(
                    it,
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    override fun `bolt12Payment`(): Bolt12Payment {
        return FfiConverterTypeBolt12Payment.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_bolt12_payment(
                    it,
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `closeChannel`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_close_channel(
                    it,
                    FfiConverterTypeUserChannelId.lower(`userChannelId`),
                    FfiConverterTypePublicKey.lower(`counterpartyNodeId`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `config`(): Config {
        return FfiConverterTypeConfig.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_config(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `connect`(`nodeId`: PublicKey, `address`: SocketAddress, `persist`: kotlin.Boolean) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_connect(
                    it,
                    FfiConverterTypePublicKey.lower(`nodeId`),
                    FfiConverterTypeSocketAddress.lower(`address`),
                    FfiConverterBoolean.lower(`persist`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(NodeException::class)
    override fun `disconnect`(`nodeId`: PublicKey) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_disconnect(
                    it,
                    FfiConverterTypePublicKey.lower(`nodeId`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(NodeException::class)
    override fun `eventHandled`() {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_event_handled(
                    it,
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(NodeException::class)
    override fun `exportPathfindingScores`(): kotlin.ByteArray {
        return FfiConverterByteArray.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_export_pathfinding_scores(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `forceCloseChannel`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey, `reason`: kotlin.String?) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_force_close_channel(
                    it,
                    FfiConverterTypeUserChannelId.lower(`userChannelId`),
                    FfiConverterTypePublicKey.lower(`counterpartyNodeId`),
                    FfiConverterOptionalString.lower(`reason`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(NodeException::class)
    override fun `getAddressBalance`(`addressStr`: kotlin.String): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_get_address_balance(
                    it,
                    FfiConverterString.lower(`addressStr`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `getTransactionDetails`(`txid`: Txid): TransactionDetails? {
        return FfiConverterOptionalTypeTransactionDetails.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_get_transaction_details(
                    it,
                    FfiConverterTypeTxid.lower(`txid`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `listBalances`(): BalanceDetails {
        return FfiConverterTypeBalanceDetails.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_list_balances(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `listChannels`(): List<ChannelDetails> {
        return FfiConverterSequenceTypeChannelDetails.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_list_channels(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `listPayments`(): List<PaymentDetails> {
        return FfiConverterSequenceTypePaymentDetails.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_list_payments(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `listPeers`(): List<PeerDetails> {
        return FfiConverterSequenceTypePeerDetails.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_list_peers(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `listeningAddresses`(): List<SocketAddress>? {
        return FfiConverterOptionalSequenceTypeSocketAddress.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_listening_addresses(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `lsps1Liquidity`(): Lsps1Liquidity {
        return FfiConverterTypeLSPS1Liquidity.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_lsps1_liquidity(
                    it,
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    override fun `networkGraph`(): NetworkGraph {
        return FfiConverterTypeNetworkGraph.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_network_graph(
                    it,
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    override fun `nextEvent`(): Event? {
        return FfiConverterOptionalTypeEvent.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_next_event(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override suspend fun `nextEventAsync`(): Event {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_next_event_async(
                    thisPtr,
                )
            },
            { future, callback, continuation -> UniffiLib.INSTANCE.ffi_ldk_node_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.INSTANCE.ffi_ldk_node_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.INSTANCE.ffi_ldk_node_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.INSTANCE.ffi_ldk_node_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterTypeEvent.lift(it) },
            // Error FFI converter
            UniffiNullRustCallStatusErrorHandler,
        )
    }

    override fun `nodeAlias`(): NodeAlias? {
        return FfiConverterOptionalTypeNodeAlias.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_node_alias(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `nodeId`(): PublicKey {
        return FfiConverterTypePublicKey.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_node_id(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `onchainPayment`(): OnchainPayment {
        return FfiConverterTypeOnchainPayment.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_onchain_payment(
                    it,
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `openAnnouncedChannel`(`nodeId`: PublicKey, `address`: SocketAddress, `channelAmountSats`: kotlin.ULong, `pushToCounterpartyMsat`: kotlin.ULong?, `channelConfig`: ChannelConfig?): UserChannelId {
        return FfiConverterTypeUserChannelId.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_open_announced_channel(
                    it,
                    FfiConverterTypePublicKey.lower(`nodeId`),
                    FfiConverterTypeSocketAddress.lower(`address`),
                    FfiConverterULong.lower(`channelAmountSats`),
                    FfiConverterOptionalULong.lower(`pushToCounterpartyMsat`),
                    FfiConverterOptionalTypeChannelConfig.lower(`channelConfig`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `openChannel`(`nodeId`: PublicKey, `address`: SocketAddress, `channelAmountSats`: kotlin.ULong, `pushToCounterpartyMsat`: kotlin.ULong?, `channelConfig`: ChannelConfig?): UserChannelId {
        return FfiConverterTypeUserChannelId.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_open_channel(
                    it,
                    FfiConverterTypePublicKey.lower(`nodeId`),
                    FfiConverterTypeSocketAddress.lower(`address`),
                    FfiConverterULong.lower(`channelAmountSats`),
                    FfiConverterOptionalULong.lower(`pushToCounterpartyMsat`),
                    FfiConverterOptionalTypeChannelConfig.lower(`channelConfig`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `payment`(`paymentId`: PaymentId): PaymentDetails? {
        return FfiConverterOptionalTypePaymentDetails.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_payment(
                    it,
                    FfiConverterTypePaymentId.lower(`paymentId`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `removePayment`(`paymentId`: PaymentId) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_remove_payment(
                    it,
                    FfiConverterTypePaymentId.lower(`paymentId`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `signMessage`(`msg`: List<kotlin.UByte>): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_sign_message(
                    it,
                    FfiConverterSequenceUByte.lower(`msg`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `spliceIn`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey, `spliceAmountSats`: kotlin.ULong) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_splice_in(
                    it,
                    FfiConverterTypeUserChannelId.lower(`userChannelId`),
                    FfiConverterTypePublicKey.lower(`counterpartyNodeId`),
                    FfiConverterULong.lower(`spliceAmountSats`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(NodeException::class)
    override fun `spliceOut`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey, `address`: Address, `spliceAmountSats`: kotlin.ULong) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_splice_out(
                    it,
                    FfiConverterTypeUserChannelId.lower(`userChannelId`),
                    FfiConverterTypePublicKey.lower(`counterpartyNodeId`),
                    FfiConverterTypeAddress.lower(`address`),
                    FfiConverterULong.lower(`spliceAmountSats`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `spontaneousPayment`(): SpontaneousPayment {
        return FfiConverterTypeSpontaneousPayment.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_spontaneous_payment(
                    it,
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `start`() {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_start(
                    it,
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `status`(): NodeStatus {
        return FfiConverterTypeNodeStatus.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_status(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `stop`() {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_stop(
                    it,
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(NodeException::class)
    override fun `syncWallets`() {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_sync_wallets(
                    it,
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `unifiedQrPayment`(): UnifiedQrPayment {
        return FfiConverterTypeUnifiedQrPayment.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_unified_qr_payment(
                    it,
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `updateChannelConfig`(`userChannelId`: UserChannelId, `counterpartyNodeId`: PublicKey, `channelConfig`: ChannelConfig) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_update_channel_config(
                    it,
                    FfiConverterTypeUserChannelId.lower(`userChannelId`),
                    FfiConverterTypePublicKey.lower(`counterpartyNodeId`),
                    FfiConverterTypeChannelConfig.lower(`channelConfig`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    override fun `verifySignature`(`msg`: List<kotlin.UByte>, `sig`: kotlin.String, `pkey`: PublicKey): kotlin.Boolean {
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_verify_signature(
                    it,
                    FfiConverterSequenceUByte.lower(`msg`),
                    FfiConverterString.lower(`sig`),
                    FfiConverterTypePublicKey.lower(`pkey`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `waitNextEvent`(): Event {
        return FfiConverterTypeEvent.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_node_wait_next_event(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }


    
    

    
    
    companion object
    
}





object FfiConverterTypeNode: FfiConverter<Node, Pointer> {

    override fun lower(value: Node): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): Node {
        return Node(value)
    }

    override fun read(buf: ByteBuffer): Node {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: Node) = 8UL

    override fun write(value: Node, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class Offer: Disposable, OfferInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_offer(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_offer(pointer!!, status)
        }!!
    }

    
    override fun `absoluteExpirySeconds`(): kotlin.ULong? {
        return FfiConverterOptionalULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_absolute_expiry_seconds(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `amount`(): OfferAmount? {
        return FfiConverterOptionalTypeOfferAmount.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_amount(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `chains`(): List<Network> {
        return FfiConverterSequenceTypeNetwork.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_chains(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `expectsQuantity`(): kotlin.Boolean {
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_expects_quantity(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `id`(): OfferId {
        return FfiConverterTypeOfferId.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_id(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `isExpired`(): kotlin.Boolean {
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_is_expired(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `isValidQuantity`(`quantity`: kotlin.ULong): kotlin.Boolean {
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_is_valid_quantity(
                    it,
                    FfiConverterULong.lower(`quantity`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `issuer`(): kotlin.String? {
        return FfiConverterOptionalString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_issuer(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `issuerSigningPubkey`(): PublicKey? {
        return FfiConverterOptionalTypePublicKey.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_issuer_signing_pubkey(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `metadata`(): List<kotlin.UByte>? {
        return FfiConverterOptionalSequenceUByte.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_metadata(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `offerDescription`(): kotlin.String? {
        return FfiConverterOptionalString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_offer_description(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `supportsChain`(`chain`: Network): kotlin.Boolean {
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_supports_chain(
                    it,
                    FfiConverterTypeNetwork.lower(`chain`),
                    uniffiRustCallStatus,
                )
            }
        })
    }


    
    
    override fun toString(): String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_uniffi_trait_display(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }
    
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is Offer) return false
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_offer_uniffi_trait_eq_eq(
                    it,
                    FfiConverterTypeOffer.lower(`other`),
                    uniffiRustCallStatus,
                )
            }
        })
    }
    

    
    companion object {
        
        @Throws(NodeException::class)
        fun `fromStr`(`offerStr`: kotlin.String): Offer {
            return FfiConverterTypeOffer.lift(uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_constructor_offer_from_str(
                    FfiConverterString.lower(`offerStr`),
                    uniffiRustCallStatus,
                )
            }!!)
        }

        
    }
    
}





object FfiConverterTypeOffer: FfiConverter<Offer, Pointer> {

    override fun lower(value: Offer): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): Offer {
        return Offer(value)
    }

    override fun read(buf: ByteBuffer): Offer {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: Offer) = 8UL

    override fun write(value: Offer, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class OnchainPayment: Disposable, OnchainPaymentInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_onchainpayment(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_onchainpayment(pointer!!, status)
        }!!
    }

    
    @Throws(NodeException::class)
    override fun `accelerateByCpfp`(`txid`: Txid, `feeRate`: FeeRate?, `destinationAddress`: Address?): Txid {
        return FfiConverterTypeTxid.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_onchainpayment_accelerate_by_cpfp(
                    it,
                    FfiConverterTypeTxid.lower(`txid`),
                    FfiConverterOptionalTypeFeeRate.lower(`feeRate`),
                    FfiConverterOptionalTypeAddress.lower(`destinationAddress`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `bumpFeeByRbf`(`txid`: Txid, `feeRate`: FeeRate): Txid {
        return FfiConverterTypeTxid.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_onchainpayment_bump_fee_by_rbf(
                    it,
                    FfiConverterTypeTxid.lower(`txid`),
                    FfiConverterTypeFeeRate.lower(`feeRate`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `calculateCpfpFeeRate`(`parentTxid`: Txid, `urgent`: kotlin.Boolean): FeeRate {
        return FfiConverterTypeFeeRate.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_onchainpayment_calculate_cpfp_fee_rate(
                    it,
                    FfiConverterTypeTxid.lower(`parentTxid`),
                    FfiConverterBoolean.lower(`urgent`),
                    uniffiRustCallStatus,
                )
            }!!
        })
    }

    @Throws(NodeException::class)
    override fun `calculateTotalFee`(`address`: Address, `amountSats`: kotlin.ULong, `feeRate`: FeeRate?, `utxosToSpend`: List<SpendableUtxo>?): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_onchainpayment_calculate_total_fee(
                    it,
                    FfiConverterTypeAddress.lower(`address`),
                    FfiConverterULong.lower(`amountSats`),
                    FfiConverterOptionalTypeFeeRate.lower(`feeRate`),
                    FfiConverterOptionalSequenceTypeSpendableUtxo.lower(`utxosToSpend`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `listSpendableOutputs`(): List<SpendableUtxo> {
        return FfiConverterSequenceTypeSpendableUtxo.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_onchainpayment_list_spendable_outputs(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `newAddress`(): Address {
        return FfiConverterTypeAddress.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_onchainpayment_new_address(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `selectUtxosWithAlgorithm`(`targetAmountSats`: kotlin.ULong, `feeRate`: FeeRate?, `algorithm`: CoinSelectionAlgorithm, `utxos`: List<SpendableUtxo>?): List<SpendableUtxo> {
        return FfiConverterSequenceTypeSpendableUtxo.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_onchainpayment_select_utxos_with_algorithm(
                    it,
                    FfiConverterULong.lower(`targetAmountSats`),
                    FfiConverterOptionalTypeFeeRate.lower(`feeRate`),
                    FfiConverterTypeCoinSelectionAlgorithm.lower(`algorithm`),
                    FfiConverterOptionalSequenceTypeSpendableUtxo.lower(`utxos`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `sendAllToAddress`(`address`: Address, `retainReserve`: kotlin.Boolean, `feeRate`: FeeRate?): Txid {
        return FfiConverterTypeTxid.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_onchainpayment_send_all_to_address(
                    it,
                    FfiConverterTypeAddress.lower(`address`),
                    FfiConverterBoolean.lower(`retainReserve`),
                    FfiConverterOptionalTypeFeeRate.lower(`feeRate`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `sendToAddress`(`address`: Address, `amountSats`: kotlin.ULong, `feeRate`: FeeRate?, `utxosToSpend`: List<SpendableUtxo>?): Txid {
        return FfiConverterTypeTxid.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_onchainpayment_send_to_address(
                    it,
                    FfiConverterTypeAddress.lower(`address`),
                    FfiConverterULong.lower(`amountSats`),
                    FfiConverterOptionalTypeFeeRate.lower(`feeRate`),
                    FfiConverterOptionalSequenceTypeSpendableUtxo.lower(`utxosToSpend`),
                    uniffiRustCallStatus,
                )
            }
        })
    }


    
    

    
    
    companion object
    
}





object FfiConverterTypeOnchainPayment: FfiConverter<OnchainPayment, Pointer> {

    override fun lower(value: OnchainPayment): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): OnchainPayment {
        return OnchainPayment(value)
    }

    override fun read(buf: ByteBuffer): OnchainPayment {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: OnchainPayment) = 8UL

    override fun write(value: OnchainPayment, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class Refund: Disposable, RefundInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_refund(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_refund(pointer!!, status)
        }!!
    }

    
    override fun `absoluteExpirySeconds`(): kotlin.ULong? {
        return FfiConverterOptionalULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_refund_absolute_expiry_seconds(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `amountMsats`(): kotlin.ULong {
        return FfiConverterULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_refund_amount_msats(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `chain`(): Network? {
        return FfiConverterOptionalTypeNetwork.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_refund_chain(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `isExpired`(): kotlin.Boolean {
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_refund_is_expired(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `issuer`(): kotlin.String? {
        return FfiConverterOptionalString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_refund_issuer(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `payerMetadata`(): List<kotlin.UByte> {
        return FfiConverterSequenceUByte.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_refund_payer_metadata(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `payerNote`(): kotlin.String? {
        return FfiConverterOptionalString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_refund_payer_note(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `payerSigningPubkey`(): PublicKey {
        return FfiConverterTypePublicKey.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_refund_payer_signing_pubkey(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `quantity`(): kotlin.ULong? {
        return FfiConverterOptionalULong.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_refund_quantity(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }

    override fun `refundDescription`(): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_refund_refund_description(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }


    
    
    override fun toString(): String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_refund_uniffi_trait_display(
                    it,
                    uniffiRustCallStatus,
                )
            }
        })
    }
    
    override fun equals(other: Any?): Boolean {
        if (this === other) return true
        if (other !is Refund) return false
        return FfiConverterBoolean.lift(callWithPointer {
            uniffiRustCall { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_refund_uniffi_trait_eq_eq(
                    it,
                    FfiConverterTypeRefund.lower(`other`),
                    uniffiRustCallStatus,
                )
            }
        })
    }
    

    
    companion object {
        
        @Throws(NodeException::class)
        fun `fromStr`(`refundStr`: kotlin.String): Refund {
            return FfiConverterTypeRefund.lift(uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_constructor_refund_from_str(
                    FfiConverterString.lower(`refundStr`),
                    uniffiRustCallStatus,
                )
            }!!)
        }

        
    }
    
}





object FfiConverterTypeRefund: FfiConverter<Refund, Pointer> {

    override fun lower(value: Refund): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): Refund {
        return Refund(value)
    }

    override fun read(buf: ByteBuffer): Refund {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: Refund) = 8UL

    override fun write(value: Refund, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class SpontaneousPayment: Disposable, SpontaneousPaymentInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_spontaneouspayment(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_spontaneouspayment(pointer!!, status)
        }!!
    }

    
    @Throws(NodeException::class)
    override fun `send`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey, `routeParameters`: RouteParametersConfig?): PaymentId {
        return FfiConverterTypePaymentId.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_spontaneouspayment_send(
                    it,
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterTypePublicKey.lower(`nodeId`),
                    FfiConverterOptionalTypeRouteParametersConfig.lower(`routeParameters`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `sendProbes`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey) {
        callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_spontaneouspayment_send_probes(
                    it,
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterTypePublicKey.lower(`nodeId`),
                    uniffiRustCallStatus,
                )
            }
        }
    }

    @Throws(NodeException::class)
    override fun `sendWithCustomTlvs`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey, `routeParameters`: RouteParametersConfig?, `customTlvs`: List<CustomTlvRecord>): PaymentId {
        return FfiConverterTypePaymentId.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_spontaneouspayment_send_with_custom_tlvs(
                    it,
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterTypePublicKey.lower(`nodeId`),
                    FfiConverterOptionalTypeRouteParametersConfig.lower(`routeParameters`),
                    FfiConverterSequenceTypeCustomTlvRecord.lower(`customTlvs`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `sendWithPreimage`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey, `preimage`: PaymentPreimage, `routeParameters`: RouteParametersConfig?): PaymentId {
        return FfiConverterTypePaymentId.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_spontaneouspayment_send_with_preimage(
                    it,
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterTypePublicKey.lower(`nodeId`),
                    FfiConverterTypePaymentPreimage.lower(`preimage`),
                    FfiConverterOptionalTypeRouteParametersConfig.lower(`routeParameters`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `sendWithPreimageAndCustomTlvs`(`amountMsat`: kotlin.ULong, `nodeId`: PublicKey, `customTlvs`: List<CustomTlvRecord>, `preimage`: PaymentPreimage, `routeParameters`: RouteParametersConfig?): PaymentId {
        return FfiConverterTypePaymentId.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_spontaneouspayment_send_with_preimage_and_custom_tlvs(
                    it,
                    FfiConverterULong.lower(`amountMsat`),
                    FfiConverterTypePublicKey.lower(`nodeId`),
                    FfiConverterSequenceTypeCustomTlvRecord.lower(`customTlvs`),
                    FfiConverterTypePaymentPreimage.lower(`preimage`),
                    FfiConverterOptionalTypeRouteParametersConfig.lower(`routeParameters`),
                    uniffiRustCallStatus,
                )
            }
        })
    }


    
    

    
    
    companion object
    
}





object FfiConverterTypeSpontaneousPayment: FfiConverter<SpontaneousPayment, Pointer> {

    override fun lower(value: SpontaneousPayment): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): SpontaneousPayment {
        return SpontaneousPayment(value)
    }

    override fun read(buf: ByteBuffer): SpontaneousPayment {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: SpontaneousPayment) = 8UL

    override fun write(value: SpontaneousPayment, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class UnifiedQrPayment: Disposable, UnifiedQrPaymentInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_unifiedqrpayment(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_unifiedqrpayment(pointer!!, status)
        }!!
    }

    
    @Throws(NodeException::class)
    override fun `receive`(`amountSats`: kotlin.ULong, `message`: kotlin.String, `expirySec`: kotlin.UInt): kotlin.String {
        return FfiConverterString.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_unifiedqrpayment_receive(
                    it,
                    FfiConverterULong.lower(`amountSats`),
                    FfiConverterString.lower(`message`),
                    FfiConverterUInt.lower(`expirySec`),
                    uniffiRustCallStatus,
                )
            }
        })
    }

    @Throws(NodeException::class)
    override fun `send`(`uriStr`: kotlin.String, `routeParameters`: RouteParametersConfig?): QrPaymentResult {
        return FfiConverterTypeQrPaymentResult.lift(callWithPointer {
            uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_unifiedqrpayment_send(
                    it,
                    FfiConverterString.lower(`uriStr`),
                    FfiConverterOptionalTypeRouteParametersConfig.lower(`routeParameters`),
                    uniffiRustCallStatus,
                )
            }
        })
    }


    
    

    
    
    companion object
    
}





object FfiConverterTypeUnifiedQrPayment: FfiConverter<UnifiedQrPayment, Pointer> {

    override fun lower(value: UnifiedQrPayment): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): UnifiedQrPayment {
        return UnifiedQrPayment(value)
    }

    override fun read(buf: ByteBuffer): UnifiedQrPayment {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: UnifiedQrPayment) = 8UL

    override fun write(value: UnifiedQrPayment, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}



open class VssHeaderProvider: Disposable, VssHeaderProviderInterface {

    constructor(pointer: Pointer) {
        this.pointer = pointer
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(pointer))
    }

    /**
     * This constructor can be used to instantiate a fake object. Only used for tests. Any
     * attempt to actually use an object constructed this way will fail as there is no
     * connected Rust object.
     */
    constructor(noPointer: NoPointer) {
        this.pointer = null
        this.cleanable = UniffiLib.CLEANER.register(this, UniffiPointerDestroyer(null))
    }

    protected val pointer: Pointer?
    protected val cleanable: UniffiCleaner.Cleanable

    private val wasDestroyed: kotlinx.atomicfu.AtomicBoolean = kotlinx.atomicfu.atomic(false)
    private val callCounter: kotlinx.atomicfu.AtomicLong = kotlinx.atomicfu.atomic(1L)

    private val lock = kotlinx.atomicfu.locks.ReentrantLock()

    private fun <T> synchronized(block: () -> T): T {
        lock.lock()
        try {
            return block()
        } finally {
            lock.unlock()
        }
    }

    override fun destroy() {
        // Only allow a single call to this method.
        // TODO: maybe we should log a warning if called more than once?
        if (this.wasDestroyed.compareAndSet(false, true)) {
            // This decrement always matches the initial count of 1 given at creation time.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    override fun close() {
        synchronized { this.destroy() }
    }

    internal inline fun <R> callWithPointer(block: (ptr: Pointer) -> R): R {
        // Check and increment the call counter, to keep the object alive.
        // This needs a compare-and-set retry loop in case of concurrent updates.
        do {
            val c = this.callCounter.value
            if (c == 0L) {
                throw IllegalStateException("${this::class::simpleName} object has already been destroyed")
            }
            if (c == Long.MAX_VALUE) {
                throw IllegalStateException("${this::class::simpleName} call counter would overflow")
            }
        } while (! this.callCounter.compareAndSet(c, c + 1L))
        // Now we can safely do the method call without the pointer being freed concurrently.
        try {
            return block(this.uniffiClonePointer())
        } finally {
            // This decrement always matches the increment we performed above.
            if (this.callCounter.decrementAndGet() == 0L) {
                cleanable.clean()
            }
        }
    }

    // Use a static inner class instead of a closure so as not to accidentally
    // capture `this` as part of the cleanable's action.
    private class UniffiPointerDestroyer(private val pointer: Pointer?) : Disposable {
        override fun destroy() {
            pointer?.let { ptr ->
                uniffiRustCall { status ->
                    UniffiLib.INSTANCE.uniffi_ldk_node_fn_free_vssheaderprovider(ptr, status)
                }
            }
        }
    }

    fun uniffiClonePointer(): Pointer {
        return uniffiRustCall { status ->
            UniffiLib.INSTANCE.uniffi_ldk_node_fn_clone_vssheaderprovider(pointer!!, status)
        }!!
    }

    
    @Throws(VssHeaderProviderException::class, kotlin.coroutines.cancellation.CancellationException::class)
    override suspend fun `getHeaders`(`request`: List<kotlin.UByte>): Map<kotlin.String, kotlin.String> {
        return uniffiRustCallAsync(
            callWithPointer { thisPtr ->
                UniffiLib.INSTANCE.uniffi_ldk_node_fn_method_vssheaderprovider_get_headers(
                    thisPtr,
                    FfiConverterSequenceUByte.lower(`request`),
                )
            },
            { future, callback, continuation -> UniffiLib.INSTANCE.ffi_ldk_node_rust_future_poll_rust_buffer(future, callback, continuation) },
            { future, continuation -> UniffiLib.INSTANCE.ffi_ldk_node_rust_future_complete_rust_buffer(future, continuation) },
            { future -> UniffiLib.INSTANCE.ffi_ldk_node_rust_future_free_rust_buffer(future) },
            { future -> UniffiLib.INSTANCE.ffi_ldk_node_rust_future_cancel_rust_buffer(future) },
            // lift function
            { FfiConverterMapStringString.lift(it) },
            // Error FFI converter
            VssHeaderProviderExceptionErrorHandler,
        )
    }


    
    

    
    
    companion object
    
}





object FfiConverterTypeVssHeaderProvider: FfiConverter<VssHeaderProvider, Pointer> {

    override fun lower(value: VssHeaderProvider): Pointer {
        return value.uniffiClonePointer()
    }

    override fun lift(value: Pointer): VssHeaderProvider {
        return VssHeaderProvider(value)
    }

    override fun read(buf: ByteBuffer): VssHeaderProvider {
        // The Rust code always writes pointers as 8 bytes, and will
        // fail to compile if they don't fit.
        return lift(buf.getLong().toPointer())
    }

    override fun allocationSize(value: VssHeaderProvider) = 8UL

    override fun write(value: VssHeaderProvider, buf: ByteBuffer) {
        // The Rust code always expects pointers written as 8 bytes,
        // and will fail to compile if they don't fit.
        buf.putLong(lower(value).toLong())
    }
}




object FfiConverterTypeAnchorChannelsConfig: FfiConverterRustBuffer<AnchorChannelsConfig> {
    override fun read(buf: ByteBuffer): AnchorChannelsConfig {
        return AnchorChannelsConfig(
            FfiConverterSequenceTypePublicKey.read(buf),
            FfiConverterULong.read(buf),
        )
    }

    override fun allocationSize(value: AnchorChannelsConfig) = (
            FfiConverterSequenceTypePublicKey.allocationSize(value.`trustedPeersNoReserve`) +
            FfiConverterULong.allocationSize(value.`perChannelReserveSats`)
    )

    override fun write(value: AnchorChannelsConfig, buf: ByteBuffer) {
        FfiConverterSequenceTypePublicKey.write(value.`trustedPeersNoReserve`, buf)
        FfiConverterULong.write(value.`perChannelReserveSats`, buf)
    }
}




object FfiConverterTypeBackgroundSyncConfig: FfiConverterRustBuffer<BackgroundSyncConfig> {
    override fun read(buf: ByteBuffer): BackgroundSyncConfig {
        return BackgroundSyncConfig(
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
        )
    }

    override fun allocationSize(value: BackgroundSyncConfig) = (
            FfiConverterULong.allocationSize(value.`onchainWalletSyncIntervalSecs`) +
            FfiConverterULong.allocationSize(value.`lightningWalletSyncIntervalSecs`) +
            FfiConverterULong.allocationSize(value.`feeRateCacheUpdateIntervalSecs`)
    )

    override fun write(value: BackgroundSyncConfig, buf: ByteBuffer) {
        FfiConverterULong.write(value.`onchainWalletSyncIntervalSecs`, buf)
        FfiConverterULong.write(value.`lightningWalletSyncIntervalSecs`, buf)
        FfiConverterULong.write(value.`feeRateCacheUpdateIntervalSecs`, buf)
    }
}




object FfiConverterTypeBalanceDetails: FfiConverterRustBuffer<BalanceDetails> {
    override fun read(buf: ByteBuffer): BalanceDetails {
        return BalanceDetails(
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterSequenceTypeLightningBalance.read(buf),
            FfiConverterSequenceTypePendingSweepBalance.read(buf),
        )
    }

    override fun allocationSize(value: BalanceDetails) = (
            FfiConverterULong.allocationSize(value.`totalOnchainBalanceSats`) +
            FfiConverterULong.allocationSize(value.`spendableOnchainBalanceSats`) +
            FfiConverterULong.allocationSize(value.`totalAnchorChannelsReserveSats`) +
            FfiConverterULong.allocationSize(value.`totalLightningBalanceSats`) +
            FfiConverterSequenceTypeLightningBalance.allocationSize(value.`lightningBalances`) +
            FfiConverterSequenceTypePendingSweepBalance.allocationSize(value.`pendingBalancesFromChannelClosures`)
    )

    override fun write(value: BalanceDetails, buf: ByteBuffer) {
        FfiConverterULong.write(value.`totalOnchainBalanceSats`, buf)
        FfiConverterULong.write(value.`spendableOnchainBalanceSats`, buf)
        FfiConverterULong.write(value.`totalAnchorChannelsReserveSats`, buf)
        FfiConverterULong.write(value.`totalLightningBalanceSats`, buf)
        FfiConverterSequenceTypeLightningBalance.write(value.`lightningBalances`, buf)
        FfiConverterSequenceTypePendingSweepBalance.write(value.`pendingBalancesFromChannelClosures`, buf)
    }
}




object FfiConverterTypeBestBlock: FfiConverterRustBuffer<BestBlock> {
    override fun read(buf: ByteBuffer): BestBlock {
        return BestBlock(
            FfiConverterTypeBlockHash.read(buf),
            FfiConverterUInt.read(buf),
        )
    }

    override fun allocationSize(value: BestBlock) = (
            FfiConverterTypeBlockHash.allocationSize(value.`blockHash`) +
            FfiConverterUInt.allocationSize(value.`height`)
    )

    override fun write(value: BestBlock, buf: ByteBuffer) {
        FfiConverterTypeBlockHash.write(value.`blockHash`, buf)
        FfiConverterUInt.write(value.`height`, buf)
    }
}




object FfiConverterTypeChannelConfig: FfiConverterRustBuffer<ChannelConfig> {
    override fun read(buf: ByteBuffer): ChannelConfig {
        return ChannelConfig(
            FfiConverterUInt.read(buf),
            FfiConverterUInt.read(buf),
            FfiConverterUShort.read(buf),
            FfiConverterTypeMaxDustHTLCExposure.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: ChannelConfig) = (
            FfiConverterUInt.allocationSize(value.`forwardingFeeProportionalMillionths`) +
            FfiConverterUInt.allocationSize(value.`forwardingFeeBaseMsat`) +
            FfiConverterUShort.allocationSize(value.`cltvExpiryDelta`) +
            FfiConverterTypeMaxDustHTLCExposure.allocationSize(value.`maxDustHtlcExposure`) +
            FfiConverterULong.allocationSize(value.`forceCloseAvoidanceMaxFeeSatoshis`) +
            FfiConverterBoolean.allocationSize(value.`acceptUnderpayingHtlcs`)
    )

    override fun write(value: ChannelConfig, buf: ByteBuffer) {
        FfiConverterUInt.write(value.`forwardingFeeProportionalMillionths`, buf)
        FfiConverterUInt.write(value.`forwardingFeeBaseMsat`, buf)
        FfiConverterUShort.write(value.`cltvExpiryDelta`, buf)
        FfiConverterTypeMaxDustHTLCExposure.write(value.`maxDustHtlcExposure`, buf)
        FfiConverterULong.write(value.`forceCloseAvoidanceMaxFeeSatoshis`, buf)
        FfiConverterBoolean.write(value.`acceptUnderpayingHtlcs`, buf)
    }
}




object FfiConverterTypeChannelDataMigration: FfiConverterRustBuffer<ChannelDataMigration> {
    override fun read(buf: ByteBuffer): ChannelDataMigration {
        return ChannelDataMigration(
            FfiConverterOptionalSequenceUByte.read(buf),
            FfiConverterSequenceSequenceUByte.read(buf),
        )
    }

    override fun allocationSize(value: ChannelDataMigration) = (
            FfiConverterOptionalSequenceUByte.allocationSize(value.`channelManager`) +
            FfiConverterSequenceSequenceUByte.allocationSize(value.`channelMonitors`)
    )

    override fun write(value: ChannelDataMigration, buf: ByteBuffer) {
        FfiConverterOptionalSequenceUByte.write(value.`channelManager`, buf)
        FfiConverterSequenceSequenceUByte.write(value.`channelMonitors`, buf)
    }
}




object FfiConverterTypeChannelDetails: FfiConverterRustBuffer<ChannelDetails> {
    override fun read(buf: ByteBuffer): ChannelDetails {
        return ChannelDetails(
            FfiConverterTypeChannelId.read(buf),
            FfiConverterTypePublicKey.read(buf),
            FfiConverterOptionalTypeOutPoint.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterTypeUserChannelId.read(buf),
            FfiConverterUInt.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterOptionalUInt.read(buf),
            FfiConverterOptionalUInt.read(buf),
            FfiConverterBoolean.read(buf),
            FfiConverterBoolean.read(buf),
            FfiConverterBoolean.read(buf),
            FfiConverterBoolean.read(buf),
            FfiConverterOptionalUShort.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalUInt.read(buf),
            FfiConverterOptionalUInt.read(buf),
            FfiConverterOptionalUShort.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterOptionalUShort.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterTypeChannelConfig.read(buf),
            FfiConverterOptionalULong.read(buf),
        )
    }

    override fun allocationSize(value: ChannelDetails) = (
            FfiConverterTypeChannelId.allocationSize(value.`channelId`) +
            FfiConverterTypePublicKey.allocationSize(value.`counterpartyNodeId`) +
            FfiConverterOptionalTypeOutPoint.allocationSize(value.`fundingTxo`) +
            FfiConverterOptionalULong.allocationSize(value.`shortChannelId`) +
            FfiConverterOptionalULong.allocationSize(value.`outboundScidAlias`) +
            FfiConverterOptionalULong.allocationSize(value.`inboundScidAlias`) +
            FfiConverterULong.allocationSize(value.`channelValueSats`) +
            FfiConverterOptionalULong.allocationSize(value.`unspendablePunishmentReserve`) +
            FfiConverterTypeUserChannelId.allocationSize(value.`userChannelId`) +
            FfiConverterUInt.allocationSize(value.`feerateSatPer1000Weight`) +
            FfiConverterULong.allocationSize(value.`outboundCapacityMsat`) +
            FfiConverterULong.allocationSize(value.`inboundCapacityMsat`) +
            FfiConverterOptionalUInt.allocationSize(value.`confirmationsRequired`) +
            FfiConverterOptionalUInt.allocationSize(value.`confirmations`) +
            FfiConverterBoolean.allocationSize(value.`isOutbound`) +
            FfiConverterBoolean.allocationSize(value.`isChannelReady`) +
            FfiConverterBoolean.allocationSize(value.`isUsable`) +
            FfiConverterBoolean.allocationSize(value.`isAnnounced`) +
            FfiConverterOptionalUShort.allocationSize(value.`cltvExpiryDelta`) +
            FfiConverterULong.allocationSize(value.`counterpartyUnspendablePunishmentReserve`) +
            FfiConverterOptionalULong.allocationSize(value.`counterpartyOutboundHtlcMinimumMsat`) +
            FfiConverterOptionalULong.allocationSize(value.`counterpartyOutboundHtlcMaximumMsat`) +
            FfiConverterOptionalUInt.allocationSize(value.`counterpartyForwardingInfoFeeBaseMsat`) +
            FfiConverterOptionalUInt.allocationSize(value.`counterpartyForwardingInfoFeeProportionalMillionths`) +
            FfiConverterOptionalUShort.allocationSize(value.`counterpartyForwardingInfoCltvExpiryDelta`) +
            FfiConverterULong.allocationSize(value.`nextOutboundHtlcLimitMsat`) +
            FfiConverterULong.allocationSize(value.`nextOutboundHtlcMinimumMsat`) +
            FfiConverterOptionalUShort.allocationSize(value.`forceCloseSpendDelay`) +
            FfiConverterULong.allocationSize(value.`inboundHtlcMinimumMsat`) +
            FfiConverterOptionalULong.allocationSize(value.`inboundHtlcMaximumMsat`) +
            FfiConverterTypeChannelConfig.allocationSize(value.`config`) +
            FfiConverterOptionalULong.allocationSize(value.`claimableOnCloseSats`)
    )

    override fun write(value: ChannelDetails, buf: ByteBuffer) {
        FfiConverterTypeChannelId.write(value.`channelId`, buf)
        FfiConverterTypePublicKey.write(value.`counterpartyNodeId`, buf)
        FfiConverterOptionalTypeOutPoint.write(value.`fundingTxo`, buf)
        FfiConverterOptionalULong.write(value.`shortChannelId`, buf)
        FfiConverterOptionalULong.write(value.`outboundScidAlias`, buf)
        FfiConverterOptionalULong.write(value.`inboundScidAlias`, buf)
        FfiConverterULong.write(value.`channelValueSats`, buf)
        FfiConverterOptionalULong.write(value.`unspendablePunishmentReserve`, buf)
        FfiConverterTypeUserChannelId.write(value.`userChannelId`, buf)
        FfiConverterUInt.write(value.`feerateSatPer1000Weight`, buf)
        FfiConverterULong.write(value.`outboundCapacityMsat`, buf)
        FfiConverterULong.write(value.`inboundCapacityMsat`, buf)
        FfiConverterOptionalUInt.write(value.`confirmationsRequired`, buf)
        FfiConverterOptionalUInt.write(value.`confirmations`, buf)
        FfiConverterBoolean.write(value.`isOutbound`, buf)
        FfiConverterBoolean.write(value.`isChannelReady`, buf)
        FfiConverterBoolean.write(value.`isUsable`, buf)
        FfiConverterBoolean.write(value.`isAnnounced`, buf)
        FfiConverterOptionalUShort.write(value.`cltvExpiryDelta`, buf)
        FfiConverterULong.write(value.`counterpartyUnspendablePunishmentReserve`, buf)
        FfiConverterOptionalULong.write(value.`counterpartyOutboundHtlcMinimumMsat`, buf)
        FfiConverterOptionalULong.write(value.`counterpartyOutboundHtlcMaximumMsat`, buf)
        FfiConverterOptionalUInt.write(value.`counterpartyForwardingInfoFeeBaseMsat`, buf)
        FfiConverterOptionalUInt.write(value.`counterpartyForwardingInfoFeeProportionalMillionths`, buf)
        FfiConverterOptionalUShort.write(value.`counterpartyForwardingInfoCltvExpiryDelta`, buf)
        FfiConverterULong.write(value.`nextOutboundHtlcLimitMsat`, buf)
        FfiConverterULong.write(value.`nextOutboundHtlcMinimumMsat`, buf)
        FfiConverterOptionalUShort.write(value.`forceCloseSpendDelay`, buf)
        FfiConverterULong.write(value.`inboundHtlcMinimumMsat`, buf)
        FfiConverterOptionalULong.write(value.`inboundHtlcMaximumMsat`, buf)
        FfiConverterTypeChannelConfig.write(value.`config`, buf)
        FfiConverterOptionalULong.write(value.`claimableOnCloseSats`, buf)
    }
}




object FfiConverterTypeChannelInfo: FfiConverterRustBuffer<ChannelInfo> {
    override fun read(buf: ByteBuffer): ChannelInfo {
        return ChannelInfo(
            FfiConverterTypeNodeId.read(buf),
            FfiConverterOptionalTypeChannelUpdateInfo.read(buf),
            FfiConverterTypeNodeId.read(buf),
            FfiConverterOptionalTypeChannelUpdateInfo.read(buf),
            FfiConverterOptionalULong.read(buf),
        )
    }

    override fun allocationSize(value: ChannelInfo) = (
            FfiConverterTypeNodeId.allocationSize(value.`nodeOne`) +
            FfiConverterOptionalTypeChannelUpdateInfo.allocationSize(value.`oneToTwo`) +
            FfiConverterTypeNodeId.allocationSize(value.`nodeTwo`) +
            FfiConverterOptionalTypeChannelUpdateInfo.allocationSize(value.`twoToOne`) +
            FfiConverterOptionalULong.allocationSize(value.`capacitySats`)
    )

    override fun write(value: ChannelInfo, buf: ByteBuffer) {
        FfiConverterTypeNodeId.write(value.`nodeOne`, buf)
        FfiConverterOptionalTypeChannelUpdateInfo.write(value.`oneToTwo`, buf)
        FfiConverterTypeNodeId.write(value.`nodeTwo`, buf)
        FfiConverterOptionalTypeChannelUpdateInfo.write(value.`twoToOne`, buf)
        FfiConverterOptionalULong.write(value.`capacitySats`, buf)
    }
}




object FfiConverterTypeChannelUpdateInfo: FfiConverterRustBuffer<ChannelUpdateInfo> {
    override fun read(buf: ByteBuffer): ChannelUpdateInfo {
        return ChannelUpdateInfo(
            FfiConverterUInt.read(buf),
            FfiConverterBoolean.read(buf),
            FfiConverterUShort.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterTypeRoutingFees.read(buf),
        )
    }

    override fun allocationSize(value: ChannelUpdateInfo) = (
            FfiConverterUInt.allocationSize(value.`lastUpdate`) +
            FfiConverterBoolean.allocationSize(value.`enabled`) +
            FfiConverterUShort.allocationSize(value.`cltvExpiryDelta`) +
            FfiConverterULong.allocationSize(value.`htlcMinimumMsat`) +
            FfiConverterULong.allocationSize(value.`htlcMaximumMsat`) +
            FfiConverterTypeRoutingFees.allocationSize(value.`fees`)
    )

    override fun write(value: ChannelUpdateInfo, buf: ByteBuffer) {
        FfiConverterUInt.write(value.`lastUpdate`, buf)
        FfiConverterBoolean.write(value.`enabled`, buf)
        FfiConverterUShort.write(value.`cltvExpiryDelta`, buf)
        FfiConverterULong.write(value.`htlcMinimumMsat`, buf)
        FfiConverterULong.write(value.`htlcMaximumMsat`, buf)
        FfiConverterTypeRoutingFees.write(value.`fees`, buf)
    }
}




object FfiConverterTypeConfig: FfiConverterRustBuffer<Config> {
    override fun read(buf: ByteBuffer): Config {
        return Config(
            FfiConverterString.read(buf),
            FfiConverterTypeNetwork.read(buf),
            FfiConverterOptionalSequenceTypeSocketAddress.read(buf),
            FfiConverterOptionalSequenceTypeSocketAddress.read(buf),
            FfiConverterOptionalTypeNodeAlias.read(buf),
            FfiConverterSequenceTypePublicKey.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterOptionalTypeAnchorChannelsConfig.read(buf),
            FfiConverterOptionalTypeRouteParametersConfig.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: Config) = (
            FfiConverterString.allocationSize(value.`storageDirPath`) +
            FfiConverterTypeNetwork.allocationSize(value.`network`) +
            FfiConverterOptionalSequenceTypeSocketAddress.allocationSize(value.`listeningAddresses`) +
            FfiConverterOptionalSequenceTypeSocketAddress.allocationSize(value.`announcementAddresses`) +
            FfiConverterOptionalTypeNodeAlias.allocationSize(value.`nodeAlias`) +
            FfiConverterSequenceTypePublicKey.allocationSize(value.`trustedPeers0conf`) +
            FfiConverterULong.allocationSize(value.`probingLiquidityLimitMultiplier`) +
            FfiConverterOptionalTypeAnchorChannelsConfig.allocationSize(value.`anchorChannelsConfig`) +
            FfiConverterOptionalTypeRouteParametersConfig.allocationSize(value.`routeParameters`) +
            FfiConverterBoolean.allocationSize(value.`includeUntrustedPendingInSpendable`)
    )

    override fun write(value: Config, buf: ByteBuffer) {
        FfiConverterString.write(value.`storageDirPath`, buf)
        FfiConverterTypeNetwork.write(value.`network`, buf)
        FfiConverterOptionalSequenceTypeSocketAddress.write(value.`listeningAddresses`, buf)
        FfiConverterOptionalSequenceTypeSocketAddress.write(value.`announcementAddresses`, buf)
        FfiConverterOptionalTypeNodeAlias.write(value.`nodeAlias`, buf)
        FfiConverterSequenceTypePublicKey.write(value.`trustedPeers0conf`, buf)
        FfiConverterULong.write(value.`probingLiquidityLimitMultiplier`, buf)
        FfiConverterOptionalTypeAnchorChannelsConfig.write(value.`anchorChannelsConfig`, buf)
        FfiConverterOptionalTypeRouteParametersConfig.write(value.`routeParameters`, buf)
        FfiConverterBoolean.write(value.`includeUntrustedPendingInSpendable`, buf)
    }
}




object FfiConverterTypeCustomTlvRecord: FfiConverterRustBuffer<CustomTlvRecord> {
    override fun read(buf: ByteBuffer): CustomTlvRecord {
        return CustomTlvRecord(
            FfiConverterULong.read(buf),
            FfiConverterSequenceUByte.read(buf),
        )
    }

    override fun allocationSize(value: CustomTlvRecord) = (
            FfiConverterULong.allocationSize(value.`typeNum`) +
            FfiConverterSequenceUByte.allocationSize(value.`value`)
    )

    override fun write(value: CustomTlvRecord, buf: ByteBuffer) {
        FfiConverterULong.write(value.`typeNum`, buf)
        FfiConverterSequenceUByte.write(value.`value`, buf)
    }
}




object FfiConverterTypeElectrumSyncConfig: FfiConverterRustBuffer<ElectrumSyncConfig> {
    override fun read(buf: ByteBuffer): ElectrumSyncConfig {
        return ElectrumSyncConfig(
            FfiConverterOptionalTypeBackgroundSyncConfig.read(buf),
        )
    }

    override fun allocationSize(value: ElectrumSyncConfig) = (
            FfiConverterOptionalTypeBackgroundSyncConfig.allocationSize(value.`backgroundSyncConfig`)
    )

    override fun write(value: ElectrumSyncConfig, buf: ByteBuffer) {
        FfiConverterOptionalTypeBackgroundSyncConfig.write(value.`backgroundSyncConfig`, buf)
    }
}




object FfiConverterTypeEsploraSyncConfig: FfiConverterRustBuffer<EsploraSyncConfig> {
    override fun read(buf: ByteBuffer): EsploraSyncConfig {
        return EsploraSyncConfig(
            FfiConverterOptionalTypeBackgroundSyncConfig.read(buf),
        )
    }

    override fun allocationSize(value: EsploraSyncConfig) = (
            FfiConverterOptionalTypeBackgroundSyncConfig.allocationSize(value.`backgroundSyncConfig`)
    )

    override fun write(value: EsploraSyncConfig, buf: ByteBuffer) {
        FfiConverterOptionalTypeBackgroundSyncConfig.write(value.`backgroundSyncConfig`, buf)
    }
}




object FfiConverterTypeLSPFeeLimits: FfiConverterRustBuffer<LspFeeLimits> {
    override fun read(buf: ByteBuffer): LspFeeLimits {
        return LspFeeLimits(
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
        )
    }

    override fun allocationSize(value: LspFeeLimits) = (
            FfiConverterOptionalULong.allocationSize(value.`maxTotalOpeningFeeMsat`) +
            FfiConverterOptionalULong.allocationSize(value.`maxProportionalOpeningFeePpmMsat`)
    )

    override fun write(value: LspFeeLimits, buf: ByteBuffer) {
        FfiConverterOptionalULong.write(value.`maxTotalOpeningFeeMsat`, buf)
        FfiConverterOptionalULong.write(value.`maxProportionalOpeningFeePpmMsat`, buf)
    }
}




object FfiConverterTypeLSPS1Bolt11PaymentInfo: FfiConverterRustBuffer<Lsps1Bolt11PaymentInfo> {
    override fun read(buf: ByteBuffer): Lsps1Bolt11PaymentInfo {
        return Lsps1Bolt11PaymentInfo(
            FfiConverterTypeLSPS1PaymentState.read(buf),
            FfiConverterTypeLSPSDateTime.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterTypeBolt11Invoice.read(buf),
        )
    }

    override fun allocationSize(value: Lsps1Bolt11PaymentInfo) = (
            FfiConverterTypeLSPS1PaymentState.allocationSize(value.`state`) +
            FfiConverterTypeLSPSDateTime.allocationSize(value.`expiresAt`) +
            FfiConverterULong.allocationSize(value.`feeTotalSat`) +
            FfiConverterULong.allocationSize(value.`orderTotalSat`) +
            FfiConverterTypeBolt11Invoice.allocationSize(value.`invoice`)
    )

    override fun write(value: Lsps1Bolt11PaymentInfo, buf: ByteBuffer) {
        FfiConverterTypeLSPS1PaymentState.write(value.`state`, buf)
        FfiConverterTypeLSPSDateTime.write(value.`expiresAt`, buf)
        FfiConverterULong.write(value.`feeTotalSat`, buf)
        FfiConverterULong.write(value.`orderTotalSat`, buf)
        FfiConverterTypeBolt11Invoice.write(value.`invoice`, buf)
    }
}




object FfiConverterTypeLSPS1ChannelInfo: FfiConverterRustBuffer<Lsps1ChannelInfo> {
    override fun read(buf: ByteBuffer): Lsps1ChannelInfo {
        return Lsps1ChannelInfo(
            FfiConverterTypeLSPSDateTime.read(buf),
            FfiConverterTypeOutPoint.read(buf),
            FfiConverterTypeLSPSDateTime.read(buf),
        )
    }

    override fun allocationSize(value: Lsps1ChannelInfo) = (
            FfiConverterTypeLSPSDateTime.allocationSize(value.`fundedAt`) +
            FfiConverterTypeOutPoint.allocationSize(value.`fundingOutpoint`) +
            FfiConverterTypeLSPSDateTime.allocationSize(value.`expiresAt`)
    )

    override fun write(value: Lsps1ChannelInfo, buf: ByteBuffer) {
        FfiConverterTypeLSPSDateTime.write(value.`fundedAt`, buf)
        FfiConverterTypeOutPoint.write(value.`fundingOutpoint`, buf)
        FfiConverterTypeLSPSDateTime.write(value.`expiresAt`, buf)
    }
}




object FfiConverterTypeLSPS1OnchainPaymentInfo: FfiConverterRustBuffer<Lsps1OnchainPaymentInfo> {
    override fun read(buf: ByteBuffer): Lsps1OnchainPaymentInfo {
        return Lsps1OnchainPaymentInfo(
            FfiConverterTypeLSPS1PaymentState.read(buf),
            FfiConverterTypeLSPSDateTime.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterTypeAddress.read(buf),
            FfiConverterOptionalUShort.read(buf),
            FfiConverterTypeFeeRate.read(buf),
            FfiConverterOptionalTypeAddress.read(buf),
        )
    }

    override fun allocationSize(value: Lsps1OnchainPaymentInfo) = (
            FfiConverterTypeLSPS1PaymentState.allocationSize(value.`state`) +
            FfiConverterTypeLSPSDateTime.allocationSize(value.`expiresAt`) +
            FfiConverterULong.allocationSize(value.`feeTotalSat`) +
            FfiConverterULong.allocationSize(value.`orderTotalSat`) +
            FfiConverterTypeAddress.allocationSize(value.`address`) +
            FfiConverterOptionalUShort.allocationSize(value.`minOnchainPaymentConfirmations`) +
            FfiConverterTypeFeeRate.allocationSize(value.`minFeeFor0conf`) +
            FfiConverterOptionalTypeAddress.allocationSize(value.`refundOnchainAddress`)
    )

    override fun write(value: Lsps1OnchainPaymentInfo, buf: ByteBuffer) {
        FfiConverterTypeLSPS1PaymentState.write(value.`state`, buf)
        FfiConverterTypeLSPSDateTime.write(value.`expiresAt`, buf)
        FfiConverterULong.write(value.`feeTotalSat`, buf)
        FfiConverterULong.write(value.`orderTotalSat`, buf)
        FfiConverterTypeAddress.write(value.`address`, buf)
        FfiConverterOptionalUShort.write(value.`minOnchainPaymentConfirmations`, buf)
        FfiConverterTypeFeeRate.write(value.`minFeeFor0conf`, buf)
        FfiConverterOptionalTypeAddress.write(value.`refundOnchainAddress`, buf)
    }
}




object FfiConverterTypeLSPS1OrderParams: FfiConverterRustBuffer<Lsps1OrderParams> {
    override fun read(buf: ByteBuffer): Lsps1OrderParams {
        return Lsps1OrderParams(
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterUShort.read(buf),
            FfiConverterUShort.read(buf),
            FfiConverterUInt.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: Lsps1OrderParams) = (
            FfiConverterULong.allocationSize(value.`lspBalanceSat`) +
            FfiConverterULong.allocationSize(value.`clientBalanceSat`) +
            FfiConverterUShort.allocationSize(value.`requiredChannelConfirmations`) +
            FfiConverterUShort.allocationSize(value.`fundingConfirmsWithinBlocks`) +
            FfiConverterUInt.allocationSize(value.`channelExpiryBlocks`) +
            FfiConverterOptionalString.allocationSize(value.`token`) +
            FfiConverterBoolean.allocationSize(value.`announceChannel`)
    )

    override fun write(value: Lsps1OrderParams, buf: ByteBuffer) {
        FfiConverterULong.write(value.`lspBalanceSat`, buf)
        FfiConverterULong.write(value.`clientBalanceSat`, buf)
        FfiConverterUShort.write(value.`requiredChannelConfirmations`, buf)
        FfiConverterUShort.write(value.`fundingConfirmsWithinBlocks`, buf)
        FfiConverterUInt.write(value.`channelExpiryBlocks`, buf)
        FfiConverterOptionalString.write(value.`token`, buf)
        FfiConverterBoolean.write(value.`announceChannel`, buf)
    }
}




object FfiConverterTypeLSPS1OrderStatus: FfiConverterRustBuffer<Lsps1OrderStatus> {
    override fun read(buf: ByteBuffer): Lsps1OrderStatus {
        return Lsps1OrderStatus(
            FfiConverterTypeLSPS1OrderId.read(buf),
            FfiConverterTypeLSPS1OrderParams.read(buf),
            FfiConverterTypeLSPS1PaymentInfo.read(buf),
            FfiConverterOptionalTypeLSPS1ChannelInfo.read(buf),
        )
    }

    override fun allocationSize(value: Lsps1OrderStatus) = (
            FfiConverterTypeLSPS1OrderId.allocationSize(value.`orderId`) +
            FfiConverterTypeLSPS1OrderParams.allocationSize(value.`orderParams`) +
            FfiConverterTypeLSPS1PaymentInfo.allocationSize(value.`paymentOptions`) +
            FfiConverterOptionalTypeLSPS1ChannelInfo.allocationSize(value.`channelState`)
    )

    override fun write(value: Lsps1OrderStatus, buf: ByteBuffer) {
        FfiConverterTypeLSPS1OrderId.write(value.`orderId`, buf)
        FfiConverterTypeLSPS1OrderParams.write(value.`orderParams`, buf)
        FfiConverterTypeLSPS1PaymentInfo.write(value.`paymentOptions`, buf)
        FfiConverterOptionalTypeLSPS1ChannelInfo.write(value.`channelState`, buf)
    }
}




object FfiConverterTypeLSPS1PaymentInfo: FfiConverterRustBuffer<Lsps1PaymentInfo> {
    override fun read(buf: ByteBuffer): Lsps1PaymentInfo {
        return Lsps1PaymentInfo(
            FfiConverterOptionalTypeLSPS1Bolt11PaymentInfo.read(buf),
            FfiConverterOptionalTypeLSPS1OnchainPaymentInfo.read(buf),
        )
    }

    override fun allocationSize(value: Lsps1PaymentInfo) = (
            FfiConverterOptionalTypeLSPS1Bolt11PaymentInfo.allocationSize(value.`bolt11`) +
            FfiConverterOptionalTypeLSPS1OnchainPaymentInfo.allocationSize(value.`onchain`)
    )

    override fun write(value: Lsps1PaymentInfo, buf: ByteBuffer) {
        FfiConverterOptionalTypeLSPS1Bolt11PaymentInfo.write(value.`bolt11`, buf)
        FfiConverterOptionalTypeLSPS1OnchainPaymentInfo.write(value.`onchain`, buf)
    }
}




object FfiConverterTypeLSPS2ServiceConfig: FfiConverterRustBuffer<Lsps2ServiceConfig> {
    override fun read(buf: ByteBuffer): Lsps2ServiceConfig {
        return Lsps2ServiceConfig(
            FfiConverterOptionalString.read(buf),
            FfiConverterBoolean.read(buf),
            FfiConverterUInt.read(buf),
            FfiConverterUInt.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterUInt.read(buf),
            FfiConverterUInt.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: Lsps2ServiceConfig) = (
            FfiConverterOptionalString.allocationSize(value.`requireToken`) +
            FfiConverterBoolean.allocationSize(value.`advertiseService`) +
            FfiConverterUInt.allocationSize(value.`channelOpeningFeePpm`) +
            FfiConverterUInt.allocationSize(value.`channelOverProvisioningPpm`) +
            FfiConverterULong.allocationSize(value.`minChannelOpeningFeeMsat`) +
            FfiConverterUInt.allocationSize(value.`minChannelLifetime`) +
            FfiConverterUInt.allocationSize(value.`maxClientToSelfDelay`) +
            FfiConverterULong.allocationSize(value.`minPaymentSizeMsat`) +
            FfiConverterULong.allocationSize(value.`maxPaymentSizeMsat`) +
            FfiConverterBoolean.allocationSize(value.`clientTrustsLsp`)
    )

    override fun write(value: Lsps2ServiceConfig, buf: ByteBuffer) {
        FfiConverterOptionalString.write(value.`requireToken`, buf)
        FfiConverterBoolean.write(value.`advertiseService`, buf)
        FfiConverterUInt.write(value.`channelOpeningFeePpm`, buf)
        FfiConverterUInt.write(value.`channelOverProvisioningPpm`, buf)
        FfiConverterULong.write(value.`minChannelOpeningFeeMsat`, buf)
        FfiConverterUInt.write(value.`minChannelLifetime`, buf)
        FfiConverterUInt.write(value.`maxClientToSelfDelay`, buf)
        FfiConverterULong.write(value.`minPaymentSizeMsat`, buf)
        FfiConverterULong.write(value.`maxPaymentSizeMsat`, buf)
        FfiConverterBoolean.write(value.`clientTrustsLsp`, buf)
    }
}




object FfiConverterTypeLogRecord: FfiConverterRustBuffer<LogRecord> {
    override fun read(buf: ByteBuffer): LogRecord {
        return LogRecord(
            FfiConverterTypeLogLevel.read(buf),
            FfiConverterString.read(buf),
            FfiConverterString.read(buf),
            FfiConverterUInt.read(buf),
        )
    }

    override fun allocationSize(value: LogRecord) = (
            FfiConverterTypeLogLevel.allocationSize(value.`level`) +
            FfiConverterString.allocationSize(value.`args`) +
            FfiConverterString.allocationSize(value.`modulePath`) +
            FfiConverterUInt.allocationSize(value.`line`)
    )

    override fun write(value: LogRecord, buf: ByteBuffer) {
        FfiConverterTypeLogLevel.write(value.`level`, buf)
        FfiConverterString.write(value.`args`, buf)
        FfiConverterString.write(value.`modulePath`, buf)
        FfiConverterUInt.write(value.`line`, buf)
    }
}




object FfiConverterTypeNodeAnnouncementInfo: FfiConverterRustBuffer<NodeAnnouncementInfo> {
    override fun read(buf: ByteBuffer): NodeAnnouncementInfo {
        return NodeAnnouncementInfo(
            FfiConverterUInt.read(buf),
            FfiConverterString.read(buf),
            FfiConverterSequenceTypeSocketAddress.read(buf),
        )
    }

    override fun allocationSize(value: NodeAnnouncementInfo) = (
            FfiConverterUInt.allocationSize(value.`lastUpdate`) +
            FfiConverterString.allocationSize(value.`alias`) +
            FfiConverterSequenceTypeSocketAddress.allocationSize(value.`addresses`)
    )

    override fun write(value: NodeAnnouncementInfo, buf: ByteBuffer) {
        FfiConverterUInt.write(value.`lastUpdate`, buf)
        FfiConverterString.write(value.`alias`, buf)
        FfiConverterSequenceTypeSocketAddress.write(value.`addresses`, buf)
    }
}




object FfiConverterTypeNodeInfo: FfiConverterRustBuffer<NodeInfo> {
    override fun read(buf: ByteBuffer): NodeInfo {
        return NodeInfo(
            FfiConverterSequenceULong.read(buf),
            FfiConverterOptionalTypeNodeAnnouncementInfo.read(buf),
        )
    }

    override fun allocationSize(value: NodeInfo) = (
            FfiConverterSequenceULong.allocationSize(value.`channels`) +
            FfiConverterOptionalTypeNodeAnnouncementInfo.allocationSize(value.`announcementInfo`)
    )

    override fun write(value: NodeInfo, buf: ByteBuffer) {
        FfiConverterSequenceULong.write(value.`channels`, buf)
        FfiConverterOptionalTypeNodeAnnouncementInfo.write(value.`announcementInfo`, buf)
    }
}




object FfiConverterTypeNodeStatus: FfiConverterRustBuffer<NodeStatus> {
    override fun read(buf: ByteBuffer): NodeStatus {
        return NodeStatus(
            FfiConverterBoolean.read(buf),
            FfiConverterTypeBestBlock.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalUInt.read(buf),
        )
    }

    override fun allocationSize(value: NodeStatus) = (
            FfiConverterBoolean.allocationSize(value.`isRunning`) +
            FfiConverterTypeBestBlock.allocationSize(value.`currentBestBlock`) +
            FfiConverterOptionalULong.allocationSize(value.`latestLightningWalletSyncTimestamp`) +
            FfiConverterOptionalULong.allocationSize(value.`latestOnchainWalletSyncTimestamp`) +
            FfiConverterOptionalULong.allocationSize(value.`latestFeeRateCacheUpdateTimestamp`) +
            FfiConverterOptionalULong.allocationSize(value.`latestRgsSnapshotTimestamp`) +
            FfiConverterOptionalULong.allocationSize(value.`latestPathfindingScoresSyncTimestamp`) +
            FfiConverterOptionalULong.allocationSize(value.`latestNodeAnnouncementBroadcastTimestamp`) +
            FfiConverterOptionalUInt.allocationSize(value.`latestChannelMonitorArchivalHeight`)
    )

    override fun write(value: NodeStatus, buf: ByteBuffer) {
        FfiConverterBoolean.write(value.`isRunning`, buf)
        FfiConverterTypeBestBlock.write(value.`currentBestBlock`, buf)
        FfiConverterOptionalULong.write(value.`latestLightningWalletSyncTimestamp`, buf)
        FfiConverterOptionalULong.write(value.`latestOnchainWalletSyncTimestamp`, buf)
        FfiConverterOptionalULong.write(value.`latestFeeRateCacheUpdateTimestamp`, buf)
        FfiConverterOptionalULong.write(value.`latestRgsSnapshotTimestamp`, buf)
        FfiConverterOptionalULong.write(value.`latestPathfindingScoresSyncTimestamp`, buf)
        FfiConverterOptionalULong.write(value.`latestNodeAnnouncementBroadcastTimestamp`, buf)
        FfiConverterOptionalUInt.write(value.`latestChannelMonitorArchivalHeight`, buf)
    }
}




object FfiConverterTypeOutPoint: FfiConverterRustBuffer<OutPoint> {
    override fun read(buf: ByteBuffer): OutPoint {
        return OutPoint(
            FfiConverterTypeTxid.read(buf),
            FfiConverterUInt.read(buf),
        )
    }

    override fun allocationSize(value: OutPoint) = (
            FfiConverterTypeTxid.allocationSize(value.`txid`) +
            FfiConverterUInt.allocationSize(value.`vout`)
    )

    override fun write(value: OutPoint, buf: ByteBuffer) {
        FfiConverterTypeTxid.write(value.`txid`, buf)
        FfiConverterUInt.write(value.`vout`, buf)
    }
}




object FfiConverterTypePaymentDetails: FfiConverterRustBuffer<PaymentDetails> {
    override fun read(buf: ByteBuffer): PaymentDetails {
        return PaymentDetails(
            FfiConverterTypePaymentId.read(buf),
            FfiConverterTypePaymentKind.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterTypePaymentDirection.read(buf),
            FfiConverterTypePaymentStatus.read(buf),
            FfiConverterULong.read(buf),
        )
    }

    override fun allocationSize(value: PaymentDetails) = (
            FfiConverterTypePaymentId.allocationSize(value.`id`) +
            FfiConverterTypePaymentKind.allocationSize(value.`kind`) +
            FfiConverterOptionalULong.allocationSize(value.`amountMsat`) +
            FfiConverterOptionalULong.allocationSize(value.`feePaidMsat`) +
            FfiConverterTypePaymentDirection.allocationSize(value.`direction`) +
            FfiConverterTypePaymentStatus.allocationSize(value.`status`) +
            FfiConverterULong.allocationSize(value.`latestUpdateTimestamp`)
    )

    override fun write(value: PaymentDetails, buf: ByteBuffer) {
        FfiConverterTypePaymentId.write(value.`id`, buf)
        FfiConverterTypePaymentKind.write(value.`kind`, buf)
        FfiConverterOptionalULong.write(value.`amountMsat`, buf)
        FfiConverterOptionalULong.write(value.`feePaidMsat`, buf)
        FfiConverterTypePaymentDirection.write(value.`direction`, buf)
        FfiConverterTypePaymentStatus.write(value.`status`, buf)
        FfiConverterULong.write(value.`latestUpdateTimestamp`, buf)
    }
}




object FfiConverterTypePeerDetails: FfiConverterRustBuffer<PeerDetails> {
    override fun read(buf: ByteBuffer): PeerDetails {
        return PeerDetails(
            FfiConverterTypePublicKey.read(buf),
            FfiConverterTypeSocketAddress.read(buf),
            FfiConverterBoolean.read(buf),
            FfiConverterBoolean.read(buf),
        )
    }

    override fun allocationSize(value: PeerDetails) = (
            FfiConverterTypePublicKey.allocationSize(value.`nodeId`) +
            FfiConverterTypeSocketAddress.allocationSize(value.`address`) +
            FfiConverterBoolean.allocationSize(value.`isPersisted`) +
            FfiConverterBoolean.allocationSize(value.`isConnected`)
    )

    override fun write(value: PeerDetails, buf: ByteBuffer) {
        FfiConverterTypePublicKey.write(value.`nodeId`, buf)
        FfiConverterTypeSocketAddress.write(value.`address`, buf)
        FfiConverterBoolean.write(value.`isPersisted`, buf)
        FfiConverterBoolean.write(value.`isConnected`, buf)
    }
}




object FfiConverterTypeRouteHintHop: FfiConverterRustBuffer<RouteHintHop> {
    override fun read(buf: ByteBuffer): RouteHintHop {
        return RouteHintHop(
            FfiConverterTypePublicKey.read(buf),
            FfiConverterULong.read(buf),
            FfiConverterUShort.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterOptionalULong.read(buf),
            FfiConverterTypeRoutingFees.read(buf),
        )
    }

    override fun allocationSize(value: RouteHintHop) = (
            FfiConverterTypePublicKey.allocationSize(value.`srcNodeId`) +
            FfiConverterULong.allocationSize(value.`shortChannelId`) +
            FfiConverterUShort.allocationSize(value.`cltvExpiryDelta`) +
            FfiConverterOptionalULong.allocationSize(value.`htlcMinimumMsat`) +
            FfiConverterOptionalULong.allocationSize(value.`htlcMaximumMsat`) +
            FfiConverterTypeRoutingFees.allocationSize(value.`fees`)
    )

    override fun write(value: RouteHintHop, buf: ByteBuffer) {
        FfiConverterTypePublicKey.write(value.`srcNodeId`, buf)
        FfiConverterULong.write(value.`shortChannelId`, buf)
        FfiConverterUShort.write(value.`cltvExpiryDelta`, buf)
        FfiConverterOptionalULong.write(value.`htlcMinimumMsat`, buf)
        FfiConverterOptionalULong.write(value.`htlcMaximumMsat`, buf)
        FfiConverterTypeRoutingFees.write(value.`fees`, buf)
    }
}




object FfiConverterTypeRouteParametersConfig: FfiConverterRustBuffer<RouteParametersConfig> {
    override fun read(buf: ByteBuffer): RouteParametersConfig {
        return RouteParametersConfig(
            FfiConverterOptionalULong.read(buf),
            FfiConverterUInt.read(buf),
            FfiConverterUByte.read(buf),
            FfiConverterUByte.read(buf),
        )
    }

    override fun allocationSize(value: RouteParametersConfig) = (
            FfiConverterOptionalULong.allocationSize(value.`maxTotalRoutingFeeMsat`) +
            FfiConverterUInt.allocationSize(value.`maxTotalCltvExpiryDelta`) +
            FfiConverterUByte.allocationSize(value.`maxPathCount`) +
            FfiConverterUByte.allocationSize(value.`maxChannelSaturationPowerOfHalf`)
    )

    override fun write(value: RouteParametersConfig, buf: ByteBuffer) {
        FfiConverterOptionalULong.write(value.`maxTotalRoutingFeeMsat`, buf)
        FfiConverterUInt.write(value.`maxTotalCltvExpiryDelta`, buf)
        FfiConverterUByte.write(value.`maxPathCount`, buf)
        FfiConverterUByte.write(value.`maxChannelSaturationPowerOfHalf`, buf)
    }
}




object FfiConverterTypeRoutingFees: FfiConverterRustBuffer<RoutingFees> {
    override fun read(buf: ByteBuffer): RoutingFees {
        return RoutingFees(
            FfiConverterUInt.read(buf),
            FfiConverterUInt.read(buf),
        )
    }

    override fun allocationSize(value: RoutingFees) = (
            FfiConverterUInt.allocationSize(value.`baseMsat`) +
            FfiConverterUInt.allocationSize(value.`proportionalMillionths`)
    )

    override fun write(value: RoutingFees, buf: ByteBuffer) {
        FfiConverterUInt.write(value.`baseMsat`, buf)
        FfiConverterUInt.write(value.`proportionalMillionths`, buf)
    }
}




object FfiConverterTypeSpendableUtxo: FfiConverterRustBuffer<SpendableUtxo> {
    override fun read(buf: ByteBuffer): SpendableUtxo {
        return SpendableUtxo(
            FfiConverterTypeOutPoint.read(buf),
            FfiConverterULong.read(buf),
        )
    }

    override fun allocationSize(value: SpendableUtxo) = (
            FfiConverterTypeOutPoint.allocationSize(value.`outpoint`) +
            FfiConverterULong.allocationSize(value.`valueSats`)
    )

    override fun write(value: SpendableUtxo, buf: ByteBuffer) {
        FfiConverterTypeOutPoint.write(value.`outpoint`, buf)
        FfiConverterULong.write(value.`valueSats`, buf)
    }
}




object FfiConverterTypeTransactionDetails: FfiConverterRustBuffer<TransactionDetails> {
    override fun read(buf: ByteBuffer): TransactionDetails {
        return TransactionDetails(
            FfiConverterLong.read(buf),
            FfiConverterSequenceTypeTxInput.read(buf),
            FfiConverterSequenceTypeTxOutput.read(buf),
        )
    }

    override fun allocationSize(value: TransactionDetails) = (
            FfiConverterLong.allocationSize(value.`amountSats`) +
            FfiConverterSequenceTypeTxInput.allocationSize(value.`inputs`) +
            FfiConverterSequenceTypeTxOutput.allocationSize(value.`outputs`)
    )

    override fun write(value: TransactionDetails, buf: ByteBuffer) {
        FfiConverterLong.write(value.`amountSats`, buf)
        FfiConverterSequenceTypeTxInput.write(value.`inputs`, buf)
        FfiConverterSequenceTypeTxOutput.write(value.`outputs`, buf)
    }
}




object FfiConverterTypeTxInput: FfiConverterRustBuffer<TxInput> {
    override fun read(buf: ByteBuffer): TxInput {
        return TxInput(
            FfiConverterTypeTxid.read(buf),
            FfiConverterUInt.read(buf),
            FfiConverterString.read(buf),
            FfiConverterSequenceString.read(buf),
            FfiConverterUInt.read(buf),
        )
    }

    override fun allocationSize(value: TxInput) = (
            FfiConverterTypeTxid.allocationSize(value.`txid`) +
            FfiConverterUInt.allocationSize(value.`vout`) +
            FfiConverterString.allocationSize(value.`scriptsig`) +
            FfiConverterSequenceString.allocationSize(value.`witness`) +
            FfiConverterUInt.allocationSize(value.`sequence`)
    )

    override fun write(value: TxInput, buf: ByteBuffer) {
        FfiConverterTypeTxid.write(value.`txid`, buf)
        FfiConverterUInt.write(value.`vout`, buf)
        FfiConverterString.write(value.`scriptsig`, buf)
        FfiConverterSequenceString.write(value.`witness`, buf)
        FfiConverterUInt.write(value.`sequence`, buf)
    }
}




object FfiConverterTypeTxOutput: FfiConverterRustBuffer<TxOutput> {
    override fun read(buf: ByteBuffer): TxOutput {
        return TxOutput(
            FfiConverterString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterOptionalString.read(buf),
            FfiConverterLong.read(buf),
            FfiConverterUInt.read(buf),
        )
    }

    override fun allocationSize(value: TxOutput) = (
            FfiConverterString.allocationSize(value.`scriptpubkey`) +
            FfiConverterOptionalString.allocationSize(value.`scriptpubkeyType`) +
            FfiConverterOptionalString.allocationSize(value.`scriptpubkeyAddress`) +
            FfiConverterLong.allocationSize(value.`value`) +
            FfiConverterUInt.allocationSize(value.`n`)
    )

    override fun write(value: TxOutput, buf: ByteBuffer) {
        FfiConverterString.write(value.`scriptpubkey`, buf)
        FfiConverterOptionalString.write(value.`scriptpubkeyType`, buf)
        FfiConverterOptionalString.write(value.`scriptpubkeyAddress`, buf)
        FfiConverterLong.write(value.`value`, buf)
        FfiConverterUInt.write(value.`n`, buf)
    }
}





object FfiConverterTypeAsyncPaymentsRole: FfiConverterRustBuffer<AsyncPaymentsRole> {
    override fun read(buf: ByteBuffer) = try {
        AsyncPaymentsRole.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: AsyncPaymentsRole) = 4UL

    override fun write(value: AsyncPaymentsRole, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





object FfiConverterTypeBalanceSource: FfiConverterRustBuffer<BalanceSource> {
    override fun read(buf: ByteBuffer) = try {
        BalanceSource.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: BalanceSource) = 4UL

    override fun write(value: BalanceSource, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





object FfiConverterTypeBolt11InvoiceDescription : FfiConverterRustBuffer<Bolt11InvoiceDescription>{
    override fun read(buf: ByteBuffer): Bolt11InvoiceDescription {
        return when(buf.getInt()) {
            1 -> Bolt11InvoiceDescription.Hash(
                FfiConverterString.read(buf),
                )
            2 -> Bolt11InvoiceDescription.Direct(
                FfiConverterString.read(buf),
                )
            else -> throw RuntimeException("invalid enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: Bolt11InvoiceDescription) = when(value) {
        is Bolt11InvoiceDescription.Hash -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterString.allocationSize(value.`hash`)
            )
        }
        is Bolt11InvoiceDescription.Direct -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterString.allocationSize(value.`description`)
            )
        }
    }

    override fun write(value: Bolt11InvoiceDescription, buf: ByteBuffer) {
        when(value) {
            is Bolt11InvoiceDescription.Hash -> {
                buf.putInt(1)
                FfiConverterString.write(value.`hash`, buf)
                Unit
            }
            is Bolt11InvoiceDescription.Direct -> {
                buf.putInt(2)
                FfiConverterString.write(value.`description`, buf)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}




object BuildExceptionErrorHandler : UniffiRustCallStatusErrorHandler<BuildException> {
    override fun lift(errorBuf: RustBufferByValue): BuildException = FfiConverterTypeBuildError.lift(errorBuf)
}

object FfiConverterTypeBuildError : FfiConverterRustBuffer<BuildException> {
    override fun read(buf: ByteBuffer): BuildException {
        return when (buf.getInt()) {
            1 -> BuildException.InvalidSeedBytes(FfiConverterString.read(buf))
            2 -> BuildException.InvalidSeedFile(FfiConverterString.read(buf))
            3 -> BuildException.InvalidSystemTime(FfiConverterString.read(buf))
            4 -> BuildException.InvalidChannelMonitor(FfiConverterString.read(buf))
            5 -> BuildException.InvalidListeningAddresses(FfiConverterString.read(buf))
            6 -> BuildException.InvalidAnnouncementAddresses(FfiConverterString.read(buf))
            7 -> BuildException.InvalidNodeAlias(FfiConverterString.read(buf))
            8 -> BuildException.RuntimeSetupFailed(FfiConverterString.read(buf))
            9 -> BuildException.ReadFailed(FfiConverterString.read(buf))
            10 -> BuildException.WriteFailed(FfiConverterString.read(buf))
            11 -> BuildException.StoragePathAccessFailed(FfiConverterString.read(buf))
            12 -> BuildException.KvStoreSetupFailed(FfiConverterString.read(buf))
            13 -> BuildException.WalletSetupFailed(FfiConverterString.read(buf))
            14 -> BuildException.LoggerSetupFailed(FfiConverterString.read(buf))
            15 -> BuildException.NetworkMismatch(FfiConverterString.read(buf))
            16 -> BuildException.AsyncPaymentsConfigMismatch(FfiConverterString.read(buf))
            else -> throw RuntimeException("invalid error enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: BuildException): ULong {
        return 4UL
    }

    override fun write(value: BuildException, buf: ByteBuffer) {
        when (value) {
            is BuildException.InvalidSeedBytes -> {
                buf.putInt(1)
                Unit
            }
            is BuildException.InvalidSeedFile -> {
                buf.putInt(2)
                Unit
            }
            is BuildException.InvalidSystemTime -> {
                buf.putInt(3)
                Unit
            }
            is BuildException.InvalidChannelMonitor -> {
                buf.putInt(4)
                Unit
            }
            is BuildException.InvalidListeningAddresses -> {
                buf.putInt(5)
                Unit
            }
            is BuildException.InvalidAnnouncementAddresses -> {
                buf.putInt(6)
                Unit
            }
            is BuildException.InvalidNodeAlias -> {
                buf.putInt(7)
                Unit
            }
            is BuildException.RuntimeSetupFailed -> {
                buf.putInt(8)
                Unit
            }
            is BuildException.ReadFailed -> {
                buf.putInt(9)
                Unit
            }
            is BuildException.WriteFailed -> {
                buf.putInt(10)
                Unit
            }
            is BuildException.StoragePathAccessFailed -> {
                buf.putInt(11)
                Unit
            }
            is BuildException.KvStoreSetupFailed -> {
                buf.putInt(12)
                Unit
            }
            is BuildException.WalletSetupFailed -> {
                buf.putInt(13)
                Unit
            }
            is BuildException.LoggerSetupFailed -> {
                buf.putInt(14)
                Unit
            }
            is BuildException.NetworkMismatch -> {
                buf.putInt(15)
                Unit
            }
            is BuildException.AsyncPaymentsConfigMismatch -> {
                buf.putInt(16)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





object FfiConverterTypeClosureReason : FfiConverterRustBuffer<ClosureReason>{
    override fun read(buf: ByteBuffer): ClosureReason {
        return when(buf.getInt()) {
            1 -> ClosureReason.CounterpartyForceClosed(
                FfiConverterTypeUntrustedString.read(buf),
                )
            2 -> ClosureReason.HolderForceClosed(
                FfiConverterOptionalBoolean.read(buf),
                FfiConverterString.read(buf),
                )
            3 -> ClosureReason.LegacyCooperativeClosure
            4 -> ClosureReason.CounterpartyInitiatedCooperativeClosure
            5 -> ClosureReason.LocallyInitiatedCooperativeClosure
            6 -> ClosureReason.CommitmentTxConfirmed
            7 -> ClosureReason.FundingTimedOut
            8 -> ClosureReason.ProcessingError(
                FfiConverterString.read(buf),
                )
            9 -> ClosureReason.DisconnectedPeer
            10 -> ClosureReason.OutdatedChannelManager
            11 -> ClosureReason.CounterpartyCoopClosedUnfundedChannel
            12 -> ClosureReason.LocallyCoopClosedUnfundedChannel
            13 -> ClosureReason.FundingBatchClosure
            14 -> ClosureReason.HtlCsTimedOut(
                FfiConverterOptionalTypePaymentHash.read(buf),
                )
            15 -> ClosureReason.PeerFeerateTooLow(
                FfiConverterUInt.read(buf),
                FfiConverterUInt.read(buf),
                )
            else -> throw RuntimeException("invalid enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: ClosureReason) = when(value) {
        is ClosureReason.CounterpartyForceClosed -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeUntrustedString.allocationSize(value.`peerMsg`)
            )
        }
        is ClosureReason.HolderForceClosed -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterOptionalBoolean.allocationSize(value.`broadcastedLatestTxn`)
                + FfiConverterString.allocationSize(value.`message`)
            )
        }
        is ClosureReason.LegacyCooperativeClosure -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
            )
        }
        is ClosureReason.CounterpartyInitiatedCooperativeClosure -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
            )
        }
        is ClosureReason.LocallyInitiatedCooperativeClosure -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
            )
        }
        is ClosureReason.CommitmentTxConfirmed -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
            )
        }
        is ClosureReason.FundingTimedOut -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
            )
        }
        is ClosureReason.ProcessingError -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterString.allocationSize(value.`err`)
            )
        }
        is ClosureReason.DisconnectedPeer -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
            )
        }
        is ClosureReason.OutdatedChannelManager -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
            )
        }
        is ClosureReason.CounterpartyCoopClosedUnfundedChannel -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
            )
        }
        is ClosureReason.LocallyCoopClosedUnfundedChannel -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
            )
        }
        is ClosureReason.FundingBatchClosure -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
            )
        }
        is ClosureReason.HtlCsTimedOut -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterOptionalTypePaymentHash.allocationSize(value.`paymentHash`)
            )
        }
        is ClosureReason.PeerFeerateTooLow -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterUInt.allocationSize(value.`peerFeerateSatPerKw`)
                + FfiConverterUInt.allocationSize(value.`requiredFeerateSatPerKw`)
            )
        }
    }

    override fun write(value: ClosureReason, buf: ByteBuffer) {
        when(value) {
            is ClosureReason.CounterpartyForceClosed -> {
                buf.putInt(1)
                FfiConverterTypeUntrustedString.write(value.`peerMsg`, buf)
                Unit
            }
            is ClosureReason.HolderForceClosed -> {
                buf.putInt(2)
                FfiConverterOptionalBoolean.write(value.`broadcastedLatestTxn`, buf)
                FfiConverterString.write(value.`message`, buf)
                Unit
            }
            is ClosureReason.LegacyCooperativeClosure -> {
                buf.putInt(3)
                Unit
            }
            is ClosureReason.CounterpartyInitiatedCooperativeClosure -> {
                buf.putInt(4)
                Unit
            }
            is ClosureReason.LocallyInitiatedCooperativeClosure -> {
                buf.putInt(5)
                Unit
            }
            is ClosureReason.CommitmentTxConfirmed -> {
                buf.putInt(6)
                Unit
            }
            is ClosureReason.FundingTimedOut -> {
                buf.putInt(7)
                Unit
            }
            is ClosureReason.ProcessingError -> {
                buf.putInt(8)
                FfiConverterString.write(value.`err`, buf)
                Unit
            }
            is ClosureReason.DisconnectedPeer -> {
                buf.putInt(9)
                Unit
            }
            is ClosureReason.OutdatedChannelManager -> {
                buf.putInt(10)
                Unit
            }
            is ClosureReason.CounterpartyCoopClosedUnfundedChannel -> {
                buf.putInt(11)
                Unit
            }
            is ClosureReason.LocallyCoopClosedUnfundedChannel -> {
                buf.putInt(12)
                Unit
            }
            is ClosureReason.FundingBatchClosure -> {
                buf.putInt(13)
                Unit
            }
            is ClosureReason.HtlCsTimedOut -> {
                buf.putInt(14)
                FfiConverterOptionalTypePaymentHash.write(value.`paymentHash`, buf)
                Unit
            }
            is ClosureReason.PeerFeerateTooLow -> {
                buf.putInt(15)
                FfiConverterUInt.write(value.`peerFeerateSatPerKw`, buf)
                FfiConverterUInt.write(value.`requiredFeerateSatPerKw`, buf)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





object FfiConverterTypeCoinSelectionAlgorithm: FfiConverterRustBuffer<CoinSelectionAlgorithm> {
    override fun read(buf: ByteBuffer) = try {
        CoinSelectionAlgorithm.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: CoinSelectionAlgorithm) = 4UL

    override fun write(value: CoinSelectionAlgorithm, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





object FfiConverterTypeConfirmationStatus : FfiConverterRustBuffer<ConfirmationStatus>{
    override fun read(buf: ByteBuffer): ConfirmationStatus {
        return when(buf.getInt()) {
            1 -> ConfirmationStatus.Confirmed(
                FfiConverterTypeBlockHash.read(buf),
                FfiConverterUInt.read(buf),
                FfiConverterULong.read(buf),
                )
            2 -> ConfirmationStatus.Unconfirmed
            else -> throw RuntimeException("invalid enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: ConfirmationStatus) = when(value) {
        is ConfirmationStatus.Confirmed -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeBlockHash.allocationSize(value.`blockHash`)
                + FfiConverterUInt.allocationSize(value.`height`)
                + FfiConverterULong.allocationSize(value.`timestamp`)
            )
        }
        is ConfirmationStatus.Unconfirmed -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
            )
        }
    }

    override fun write(value: ConfirmationStatus, buf: ByteBuffer) {
        when(value) {
            is ConfirmationStatus.Confirmed -> {
                buf.putInt(1)
                FfiConverterTypeBlockHash.write(value.`blockHash`, buf)
                FfiConverterUInt.write(value.`height`, buf)
                FfiConverterULong.write(value.`timestamp`, buf)
                Unit
            }
            is ConfirmationStatus.Unconfirmed -> {
                buf.putInt(2)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





object FfiConverterTypeCurrency: FfiConverterRustBuffer<Currency> {
    override fun read(buf: ByteBuffer) = try {
        Currency.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: Currency) = 4UL

    override fun write(value: Currency, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





object FfiConverterTypeEvent : FfiConverterRustBuffer<Event>{
    override fun read(buf: ByteBuffer): Event {
        return when(buf.getInt()) {
            1 -> Event.PaymentSuccessful(
                FfiConverterOptionalTypePaymentId.read(buf),
                FfiConverterTypePaymentHash.read(buf),
                FfiConverterOptionalTypePaymentPreimage.read(buf),
                FfiConverterOptionalULong.read(buf),
                )
            2 -> Event.PaymentFailed(
                FfiConverterOptionalTypePaymentId.read(buf),
                FfiConverterOptionalTypePaymentHash.read(buf),
                FfiConverterOptionalTypePaymentFailureReason.read(buf),
                )
            3 -> Event.PaymentReceived(
                FfiConverterOptionalTypePaymentId.read(buf),
                FfiConverterTypePaymentHash.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterSequenceTypeCustomTlvRecord.read(buf),
                )
            4 -> Event.PaymentClaimable(
                FfiConverterTypePaymentId.read(buf),
                FfiConverterTypePaymentHash.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterOptionalUInt.read(buf),
                FfiConverterSequenceTypeCustomTlvRecord.read(buf),
                )
            5 -> Event.PaymentForwarded(
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypeChannelId.read(buf),
                FfiConverterOptionalTypeUserChannelId.read(buf),
                FfiConverterOptionalTypeUserChannelId.read(buf),
                FfiConverterOptionalTypePublicKey.read(buf),
                FfiConverterOptionalTypePublicKey.read(buf),
                FfiConverterOptionalULong.read(buf),
                FfiConverterOptionalULong.read(buf),
                FfiConverterBoolean.read(buf),
                FfiConverterOptionalULong.read(buf),
                )
            6 -> Event.ChannelPending(
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypeUserChannelId.read(buf),
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypePublicKey.read(buf),
                FfiConverterTypeOutPoint.read(buf),
                )
            7 -> Event.ChannelReady(
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypeUserChannelId.read(buf),
                FfiConverterOptionalTypePublicKey.read(buf),
                FfiConverterOptionalTypeOutPoint.read(buf),
                )
            8 -> Event.ChannelClosed(
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypeUserChannelId.read(buf),
                FfiConverterOptionalTypePublicKey.read(buf),
                FfiConverterOptionalTypeClosureReason.read(buf),
                )
            9 -> Event.SplicePending(
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypeUserChannelId.read(buf),
                FfiConverterTypePublicKey.read(buf),
                FfiConverterTypeOutPoint.read(buf),
                )
            10 -> Event.SpliceFailed(
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypeUserChannelId.read(buf),
                FfiConverterTypePublicKey.read(buf),
                FfiConverterOptionalTypeOutPoint.read(buf),
                )
            11 -> Event.OnchainTransactionConfirmed(
                FfiConverterTypeTxid.read(buf),
                FfiConverterTypeBlockHash.read(buf),
                FfiConverterUInt.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterTypeTransactionDetails.read(buf),
                )
            12 -> Event.OnchainTransactionReceived(
                FfiConverterTypeTxid.read(buf),
                FfiConverterTypeTransactionDetails.read(buf),
                )
            13 -> Event.OnchainTransactionReplaced(
                FfiConverterTypeTxid.read(buf),
                FfiConverterSequenceTypeTxid.read(buf),
                )
            14 -> Event.OnchainTransactionReorged(
                FfiConverterTypeTxid.read(buf),
                )
            15 -> Event.OnchainTransactionEvicted(
                FfiConverterTypeTxid.read(buf),
                )
            16 -> Event.SyncProgress(
                FfiConverterTypeSyncType.read(buf),
                FfiConverterUByte.read(buf),
                FfiConverterUInt.read(buf),
                FfiConverterUInt.read(buf),
                )
            17 -> Event.SyncCompleted(
                FfiConverterTypeSyncType.read(buf),
                FfiConverterUInt.read(buf),
                )
            18 -> Event.BalanceChanged(
                FfiConverterULong.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterULong.read(buf),
                )
            else -> throw RuntimeException("invalid enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: Event) = when(value) {
        is Event.PaymentSuccessful -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterOptionalTypePaymentId.allocationSize(value.`paymentId`)
                + FfiConverterTypePaymentHash.allocationSize(value.`paymentHash`)
                + FfiConverterOptionalTypePaymentPreimage.allocationSize(value.`paymentPreimage`)
                + FfiConverterOptionalULong.allocationSize(value.`feePaidMsat`)
            )
        }
        is Event.PaymentFailed -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterOptionalTypePaymentId.allocationSize(value.`paymentId`)
                + FfiConverterOptionalTypePaymentHash.allocationSize(value.`paymentHash`)
                + FfiConverterOptionalTypePaymentFailureReason.allocationSize(value.`reason`)
            )
        }
        is Event.PaymentReceived -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterOptionalTypePaymentId.allocationSize(value.`paymentId`)
                + FfiConverterTypePaymentHash.allocationSize(value.`paymentHash`)
                + FfiConverterULong.allocationSize(value.`amountMsat`)
                + FfiConverterSequenceTypeCustomTlvRecord.allocationSize(value.`customRecords`)
            )
        }
        is Event.PaymentClaimable -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypePaymentId.allocationSize(value.`paymentId`)
                + FfiConverterTypePaymentHash.allocationSize(value.`paymentHash`)
                + FfiConverterULong.allocationSize(value.`claimableAmountMsat`)
                + FfiConverterOptionalUInt.allocationSize(value.`claimDeadline`)
                + FfiConverterSequenceTypeCustomTlvRecord.allocationSize(value.`customRecords`)
            )
        }
        is Event.PaymentForwarded -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeChannelId.allocationSize(value.`prevChannelId`)
                + FfiConverterTypeChannelId.allocationSize(value.`nextChannelId`)
                + FfiConverterOptionalTypeUserChannelId.allocationSize(value.`prevUserChannelId`)
                + FfiConverterOptionalTypeUserChannelId.allocationSize(value.`nextUserChannelId`)
                + FfiConverterOptionalTypePublicKey.allocationSize(value.`prevNodeId`)
                + FfiConverterOptionalTypePublicKey.allocationSize(value.`nextNodeId`)
                + FfiConverterOptionalULong.allocationSize(value.`totalFeeEarnedMsat`)
                + FfiConverterOptionalULong.allocationSize(value.`skimmedFeeMsat`)
                + FfiConverterBoolean.allocationSize(value.`claimFromOnchainTx`)
                + FfiConverterOptionalULong.allocationSize(value.`outboundAmountForwardedMsat`)
            )
        }
        is Event.ChannelPending -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterTypeUserChannelId.allocationSize(value.`userChannelId`)
                + FfiConverterTypeChannelId.allocationSize(value.`formerTemporaryChannelId`)
                + FfiConverterTypePublicKey.allocationSize(value.`counterpartyNodeId`)
                + FfiConverterTypeOutPoint.allocationSize(value.`fundingTxo`)
            )
        }
        is Event.ChannelReady -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterTypeUserChannelId.allocationSize(value.`userChannelId`)
                + FfiConverterOptionalTypePublicKey.allocationSize(value.`counterpartyNodeId`)
                + FfiConverterOptionalTypeOutPoint.allocationSize(value.`fundingTxo`)
            )
        }
        is Event.ChannelClosed -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterTypeUserChannelId.allocationSize(value.`userChannelId`)
                + FfiConverterOptionalTypePublicKey.allocationSize(value.`counterpartyNodeId`)
                + FfiConverterOptionalTypeClosureReason.allocationSize(value.`reason`)
            )
        }
        is Event.SplicePending -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterTypeUserChannelId.allocationSize(value.`userChannelId`)
                + FfiConverterTypePublicKey.allocationSize(value.`counterpartyNodeId`)
                + FfiConverterTypeOutPoint.allocationSize(value.`newFundingTxo`)
            )
        }
        is Event.SpliceFailed -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterTypeUserChannelId.allocationSize(value.`userChannelId`)
                + FfiConverterTypePublicKey.allocationSize(value.`counterpartyNodeId`)
                + FfiConverterOptionalTypeOutPoint.allocationSize(value.`abandonedFundingTxo`)
            )
        }
        is Event.OnchainTransactionConfirmed -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeTxid.allocationSize(value.`txid`)
                + FfiConverterTypeBlockHash.allocationSize(value.`blockHash`)
                + FfiConverterUInt.allocationSize(value.`blockHeight`)
                + FfiConverterULong.allocationSize(value.`confirmationTime`)
                + FfiConverterTypeTransactionDetails.allocationSize(value.`details`)
            )
        }
        is Event.OnchainTransactionReceived -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeTxid.allocationSize(value.`txid`)
                + FfiConverterTypeTransactionDetails.allocationSize(value.`details`)
            )
        }
        is Event.OnchainTransactionReplaced -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeTxid.allocationSize(value.`txid`)
                + FfiConverterSequenceTypeTxid.allocationSize(value.`conflicts`)
            )
        }
        is Event.OnchainTransactionReorged -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeTxid.allocationSize(value.`txid`)
            )
        }
        is Event.OnchainTransactionEvicted -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeTxid.allocationSize(value.`txid`)
            )
        }
        is Event.SyncProgress -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeSyncType.allocationSize(value.`syncType`)
                + FfiConverterUByte.allocationSize(value.`progressPercent`)
                + FfiConverterUInt.allocationSize(value.`currentBlockHeight`)
                + FfiConverterUInt.allocationSize(value.`targetBlockHeight`)
            )
        }
        is Event.SyncCompleted -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeSyncType.allocationSize(value.`syncType`)
                + FfiConverterUInt.allocationSize(value.`syncedBlockHeight`)
            )
        }
        is Event.BalanceChanged -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterULong.allocationSize(value.`oldSpendableOnchainBalanceSats`)
                + FfiConverterULong.allocationSize(value.`newSpendableOnchainBalanceSats`)
                + FfiConverterULong.allocationSize(value.`oldTotalOnchainBalanceSats`)
                + FfiConverterULong.allocationSize(value.`newTotalOnchainBalanceSats`)
                + FfiConverterULong.allocationSize(value.`oldTotalLightningBalanceSats`)
                + FfiConverterULong.allocationSize(value.`newTotalLightningBalanceSats`)
            )
        }
    }

    override fun write(value: Event, buf: ByteBuffer) {
        when(value) {
            is Event.PaymentSuccessful -> {
                buf.putInt(1)
                FfiConverterOptionalTypePaymentId.write(value.`paymentId`, buf)
                FfiConverterTypePaymentHash.write(value.`paymentHash`, buf)
                FfiConverterOptionalTypePaymentPreimage.write(value.`paymentPreimage`, buf)
                FfiConverterOptionalULong.write(value.`feePaidMsat`, buf)
                Unit
            }
            is Event.PaymentFailed -> {
                buf.putInt(2)
                FfiConverterOptionalTypePaymentId.write(value.`paymentId`, buf)
                FfiConverterOptionalTypePaymentHash.write(value.`paymentHash`, buf)
                FfiConverterOptionalTypePaymentFailureReason.write(value.`reason`, buf)
                Unit
            }
            is Event.PaymentReceived -> {
                buf.putInt(3)
                FfiConverterOptionalTypePaymentId.write(value.`paymentId`, buf)
                FfiConverterTypePaymentHash.write(value.`paymentHash`, buf)
                FfiConverterULong.write(value.`amountMsat`, buf)
                FfiConverterSequenceTypeCustomTlvRecord.write(value.`customRecords`, buf)
                Unit
            }
            is Event.PaymentClaimable -> {
                buf.putInt(4)
                FfiConverterTypePaymentId.write(value.`paymentId`, buf)
                FfiConverterTypePaymentHash.write(value.`paymentHash`, buf)
                FfiConverterULong.write(value.`claimableAmountMsat`, buf)
                FfiConverterOptionalUInt.write(value.`claimDeadline`, buf)
                FfiConverterSequenceTypeCustomTlvRecord.write(value.`customRecords`, buf)
                Unit
            }
            is Event.PaymentForwarded -> {
                buf.putInt(5)
                FfiConverterTypeChannelId.write(value.`prevChannelId`, buf)
                FfiConverterTypeChannelId.write(value.`nextChannelId`, buf)
                FfiConverterOptionalTypeUserChannelId.write(value.`prevUserChannelId`, buf)
                FfiConverterOptionalTypeUserChannelId.write(value.`nextUserChannelId`, buf)
                FfiConverterOptionalTypePublicKey.write(value.`prevNodeId`, buf)
                FfiConverterOptionalTypePublicKey.write(value.`nextNodeId`, buf)
                FfiConverterOptionalULong.write(value.`totalFeeEarnedMsat`, buf)
                FfiConverterOptionalULong.write(value.`skimmedFeeMsat`, buf)
                FfiConverterBoolean.write(value.`claimFromOnchainTx`, buf)
                FfiConverterOptionalULong.write(value.`outboundAmountForwardedMsat`, buf)
                Unit
            }
            is Event.ChannelPending -> {
                buf.putInt(6)
                FfiConverterTypeChannelId.write(value.`channelId`, buf)
                FfiConverterTypeUserChannelId.write(value.`userChannelId`, buf)
                FfiConverterTypeChannelId.write(value.`formerTemporaryChannelId`, buf)
                FfiConverterTypePublicKey.write(value.`counterpartyNodeId`, buf)
                FfiConverterTypeOutPoint.write(value.`fundingTxo`, buf)
                Unit
            }
            is Event.ChannelReady -> {
                buf.putInt(7)
                FfiConverterTypeChannelId.write(value.`channelId`, buf)
                FfiConverterTypeUserChannelId.write(value.`userChannelId`, buf)
                FfiConverterOptionalTypePublicKey.write(value.`counterpartyNodeId`, buf)
                FfiConverterOptionalTypeOutPoint.write(value.`fundingTxo`, buf)
                Unit
            }
            is Event.ChannelClosed -> {
                buf.putInt(8)
                FfiConverterTypeChannelId.write(value.`channelId`, buf)
                FfiConverterTypeUserChannelId.write(value.`userChannelId`, buf)
                FfiConverterOptionalTypePublicKey.write(value.`counterpartyNodeId`, buf)
                FfiConverterOptionalTypeClosureReason.write(value.`reason`, buf)
                Unit
            }
            is Event.SplicePending -> {
                buf.putInt(9)
                FfiConverterTypeChannelId.write(value.`channelId`, buf)
                FfiConverterTypeUserChannelId.write(value.`userChannelId`, buf)
                FfiConverterTypePublicKey.write(value.`counterpartyNodeId`, buf)
                FfiConverterTypeOutPoint.write(value.`newFundingTxo`, buf)
                Unit
            }
            is Event.SpliceFailed -> {
                buf.putInt(10)
                FfiConverterTypeChannelId.write(value.`channelId`, buf)
                FfiConverterTypeUserChannelId.write(value.`userChannelId`, buf)
                FfiConverterTypePublicKey.write(value.`counterpartyNodeId`, buf)
                FfiConverterOptionalTypeOutPoint.write(value.`abandonedFundingTxo`, buf)
                Unit
            }
            is Event.OnchainTransactionConfirmed -> {
                buf.putInt(11)
                FfiConverterTypeTxid.write(value.`txid`, buf)
                FfiConverterTypeBlockHash.write(value.`blockHash`, buf)
                FfiConverterUInt.write(value.`blockHeight`, buf)
                FfiConverterULong.write(value.`confirmationTime`, buf)
                FfiConverterTypeTransactionDetails.write(value.`details`, buf)
                Unit
            }
            is Event.OnchainTransactionReceived -> {
                buf.putInt(12)
                FfiConverterTypeTxid.write(value.`txid`, buf)
                FfiConverterTypeTransactionDetails.write(value.`details`, buf)
                Unit
            }
            is Event.OnchainTransactionReplaced -> {
                buf.putInt(13)
                FfiConverterTypeTxid.write(value.`txid`, buf)
                FfiConverterSequenceTypeTxid.write(value.`conflicts`, buf)
                Unit
            }
            is Event.OnchainTransactionReorged -> {
                buf.putInt(14)
                FfiConverterTypeTxid.write(value.`txid`, buf)
                Unit
            }
            is Event.OnchainTransactionEvicted -> {
                buf.putInt(15)
                FfiConverterTypeTxid.write(value.`txid`, buf)
                Unit
            }
            is Event.SyncProgress -> {
                buf.putInt(16)
                FfiConverterTypeSyncType.write(value.`syncType`, buf)
                FfiConverterUByte.write(value.`progressPercent`, buf)
                FfiConverterUInt.write(value.`currentBlockHeight`, buf)
                FfiConverterUInt.write(value.`targetBlockHeight`, buf)
                Unit
            }
            is Event.SyncCompleted -> {
                buf.putInt(17)
                FfiConverterTypeSyncType.write(value.`syncType`, buf)
                FfiConverterUInt.write(value.`syncedBlockHeight`, buf)
                Unit
            }
            is Event.BalanceChanged -> {
                buf.putInt(18)
                FfiConverterULong.write(value.`oldSpendableOnchainBalanceSats`, buf)
                FfiConverterULong.write(value.`newSpendableOnchainBalanceSats`, buf)
                FfiConverterULong.write(value.`oldTotalOnchainBalanceSats`, buf)
                FfiConverterULong.write(value.`newTotalOnchainBalanceSats`, buf)
                FfiConverterULong.write(value.`oldTotalLightningBalanceSats`, buf)
                FfiConverterULong.write(value.`newTotalLightningBalanceSats`, buf)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





object FfiConverterTypeLSPS1PaymentState: FfiConverterRustBuffer<Lsps1PaymentState> {
    override fun read(buf: ByteBuffer) = try {
        Lsps1PaymentState.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: Lsps1PaymentState) = 4UL

    override fun write(value: Lsps1PaymentState, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





object FfiConverterTypeLightningBalance : FfiConverterRustBuffer<LightningBalance>{
    override fun read(buf: ByteBuffer): LightningBalance {
        return when(buf.getInt()) {
            1 -> LightningBalance.ClaimableOnChannelClose(
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypePublicKey.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterULong.read(buf),
                )
            2 -> LightningBalance.ClaimableAwaitingConfirmations(
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypePublicKey.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterUInt.read(buf),
                FfiConverterTypeBalanceSource.read(buf),
                )
            3 -> LightningBalance.ContentiousClaimable(
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypePublicKey.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterUInt.read(buf),
                FfiConverterTypePaymentHash.read(buf),
                FfiConverterTypePaymentPreimage.read(buf),
                )
            4 -> LightningBalance.MaybeTimeoutClaimableHtlc(
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypePublicKey.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterUInt.read(buf),
                FfiConverterTypePaymentHash.read(buf),
                FfiConverterBoolean.read(buf),
                )
            5 -> LightningBalance.MaybePreimageClaimableHtlc(
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypePublicKey.read(buf),
                FfiConverterULong.read(buf),
                FfiConverterUInt.read(buf),
                FfiConverterTypePaymentHash.read(buf),
                )
            6 -> LightningBalance.CounterpartyRevokedOutputClaimable(
                FfiConverterTypeChannelId.read(buf),
                FfiConverterTypePublicKey.read(buf),
                FfiConverterULong.read(buf),
                )
            else -> throw RuntimeException("invalid enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: LightningBalance) = when(value) {
        is LightningBalance.ClaimableOnChannelClose -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterTypePublicKey.allocationSize(value.`counterpartyNodeId`)
                + FfiConverterULong.allocationSize(value.`amountSatoshis`)
                + FfiConverterULong.allocationSize(value.`transactionFeeSatoshis`)
                + FfiConverterULong.allocationSize(value.`outboundPaymentHtlcRoundedMsat`)
                + FfiConverterULong.allocationSize(value.`outboundForwardedHtlcRoundedMsat`)
                + FfiConverterULong.allocationSize(value.`inboundClaimingHtlcRoundedMsat`)
                + FfiConverterULong.allocationSize(value.`inboundHtlcRoundedMsat`)
            )
        }
        is LightningBalance.ClaimableAwaitingConfirmations -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterTypePublicKey.allocationSize(value.`counterpartyNodeId`)
                + FfiConverterULong.allocationSize(value.`amountSatoshis`)
                + FfiConverterUInt.allocationSize(value.`confirmationHeight`)
                + FfiConverterTypeBalanceSource.allocationSize(value.`source`)
            )
        }
        is LightningBalance.ContentiousClaimable -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterTypePublicKey.allocationSize(value.`counterpartyNodeId`)
                + FfiConverterULong.allocationSize(value.`amountSatoshis`)
                + FfiConverterUInt.allocationSize(value.`timeoutHeight`)
                + FfiConverterTypePaymentHash.allocationSize(value.`paymentHash`)
                + FfiConverterTypePaymentPreimage.allocationSize(value.`paymentPreimage`)
            )
        }
        is LightningBalance.MaybeTimeoutClaimableHtlc -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterTypePublicKey.allocationSize(value.`counterpartyNodeId`)
                + FfiConverterULong.allocationSize(value.`amountSatoshis`)
                + FfiConverterUInt.allocationSize(value.`claimableHeight`)
                + FfiConverterTypePaymentHash.allocationSize(value.`paymentHash`)
                + FfiConverterBoolean.allocationSize(value.`outboundPayment`)
            )
        }
        is LightningBalance.MaybePreimageClaimableHtlc -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterTypePublicKey.allocationSize(value.`counterpartyNodeId`)
                + FfiConverterULong.allocationSize(value.`amountSatoshis`)
                + FfiConverterUInt.allocationSize(value.`expiryHeight`)
                + FfiConverterTypePaymentHash.allocationSize(value.`paymentHash`)
            )
        }
        is LightningBalance.CounterpartyRevokedOutputClaimable -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterTypePublicKey.allocationSize(value.`counterpartyNodeId`)
                + FfiConverterULong.allocationSize(value.`amountSatoshis`)
            )
        }
    }

    override fun write(value: LightningBalance, buf: ByteBuffer) {
        when(value) {
            is LightningBalance.ClaimableOnChannelClose -> {
                buf.putInt(1)
                FfiConverterTypeChannelId.write(value.`channelId`, buf)
                FfiConverterTypePublicKey.write(value.`counterpartyNodeId`, buf)
                FfiConverterULong.write(value.`amountSatoshis`, buf)
                FfiConverterULong.write(value.`transactionFeeSatoshis`, buf)
                FfiConverterULong.write(value.`outboundPaymentHtlcRoundedMsat`, buf)
                FfiConverterULong.write(value.`outboundForwardedHtlcRoundedMsat`, buf)
                FfiConverterULong.write(value.`inboundClaimingHtlcRoundedMsat`, buf)
                FfiConverterULong.write(value.`inboundHtlcRoundedMsat`, buf)
                Unit
            }
            is LightningBalance.ClaimableAwaitingConfirmations -> {
                buf.putInt(2)
                FfiConverterTypeChannelId.write(value.`channelId`, buf)
                FfiConverterTypePublicKey.write(value.`counterpartyNodeId`, buf)
                FfiConverterULong.write(value.`amountSatoshis`, buf)
                FfiConverterUInt.write(value.`confirmationHeight`, buf)
                FfiConverterTypeBalanceSource.write(value.`source`, buf)
                Unit
            }
            is LightningBalance.ContentiousClaimable -> {
                buf.putInt(3)
                FfiConverterTypeChannelId.write(value.`channelId`, buf)
                FfiConverterTypePublicKey.write(value.`counterpartyNodeId`, buf)
                FfiConverterULong.write(value.`amountSatoshis`, buf)
                FfiConverterUInt.write(value.`timeoutHeight`, buf)
                FfiConverterTypePaymentHash.write(value.`paymentHash`, buf)
                FfiConverterTypePaymentPreimage.write(value.`paymentPreimage`, buf)
                Unit
            }
            is LightningBalance.MaybeTimeoutClaimableHtlc -> {
                buf.putInt(4)
                FfiConverterTypeChannelId.write(value.`channelId`, buf)
                FfiConverterTypePublicKey.write(value.`counterpartyNodeId`, buf)
                FfiConverterULong.write(value.`amountSatoshis`, buf)
                FfiConverterUInt.write(value.`claimableHeight`, buf)
                FfiConverterTypePaymentHash.write(value.`paymentHash`, buf)
                FfiConverterBoolean.write(value.`outboundPayment`, buf)
                Unit
            }
            is LightningBalance.MaybePreimageClaimableHtlc -> {
                buf.putInt(5)
                FfiConverterTypeChannelId.write(value.`channelId`, buf)
                FfiConverterTypePublicKey.write(value.`counterpartyNodeId`, buf)
                FfiConverterULong.write(value.`amountSatoshis`, buf)
                FfiConverterUInt.write(value.`expiryHeight`, buf)
                FfiConverterTypePaymentHash.write(value.`paymentHash`, buf)
                Unit
            }
            is LightningBalance.CounterpartyRevokedOutputClaimable -> {
                buf.putInt(6)
                FfiConverterTypeChannelId.write(value.`channelId`, buf)
                FfiConverterTypePublicKey.write(value.`counterpartyNodeId`, buf)
                FfiConverterULong.write(value.`amountSatoshis`, buf)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





object FfiConverterTypeLogLevel: FfiConverterRustBuffer<LogLevel> {
    override fun read(buf: ByteBuffer) = try {
        LogLevel.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: LogLevel) = 4UL

    override fun write(value: LogLevel, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





object FfiConverterTypeMaxDustHTLCExposure : FfiConverterRustBuffer<MaxDustHtlcExposure>{
    override fun read(buf: ByteBuffer): MaxDustHtlcExposure {
        return when(buf.getInt()) {
            1 -> MaxDustHtlcExposure.FixedLimit(
                FfiConverterULong.read(buf),
                )
            2 -> MaxDustHtlcExposure.FeeRateMultiplier(
                FfiConverterULong.read(buf),
                )
            else -> throw RuntimeException("invalid enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: MaxDustHtlcExposure) = when(value) {
        is MaxDustHtlcExposure.FixedLimit -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterULong.allocationSize(value.`limitMsat`)
            )
        }
        is MaxDustHtlcExposure.FeeRateMultiplier -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterULong.allocationSize(value.`multiplier`)
            )
        }
    }

    override fun write(value: MaxDustHtlcExposure, buf: ByteBuffer) {
        when(value) {
            is MaxDustHtlcExposure.FixedLimit -> {
                buf.putInt(1)
                FfiConverterULong.write(value.`limitMsat`, buf)
                Unit
            }
            is MaxDustHtlcExposure.FeeRateMultiplier -> {
                buf.putInt(2)
                FfiConverterULong.write(value.`multiplier`, buf)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





object FfiConverterTypeNetwork: FfiConverterRustBuffer<Network> {
    override fun read(buf: ByteBuffer) = try {
        Network.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: Network) = 4UL

    override fun write(value: Network, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}




object NodeExceptionErrorHandler : UniffiRustCallStatusErrorHandler<NodeException> {
    override fun lift(errorBuf: RustBufferByValue): NodeException = FfiConverterTypeNodeError.lift(errorBuf)
}

object FfiConverterTypeNodeError : FfiConverterRustBuffer<NodeException> {
    override fun read(buf: ByteBuffer): NodeException {
        return when (buf.getInt()) {
            1 -> NodeException.AlreadyRunning(FfiConverterString.read(buf))
            2 -> NodeException.NotRunning(FfiConverterString.read(buf))
            3 -> NodeException.OnchainTxCreationFailed(FfiConverterString.read(buf))
            4 -> NodeException.ConnectionFailed(FfiConverterString.read(buf))
            5 -> NodeException.InvoiceCreationFailed(FfiConverterString.read(buf))
            6 -> NodeException.InvoiceRequestCreationFailed(FfiConverterString.read(buf))
            7 -> NodeException.OfferCreationFailed(FfiConverterString.read(buf))
            8 -> NodeException.RefundCreationFailed(FfiConverterString.read(buf))
            9 -> NodeException.PaymentSendingFailed(FfiConverterString.read(buf))
            10 -> NodeException.InvalidCustomTlvs(FfiConverterString.read(buf))
            11 -> NodeException.ProbeSendingFailed(FfiConverterString.read(buf))
            12 -> NodeException.RouteNotFound(FfiConverterString.read(buf))
            13 -> NodeException.ChannelCreationFailed(FfiConverterString.read(buf))
            14 -> NodeException.ChannelClosingFailed(FfiConverterString.read(buf))
            15 -> NodeException.ChannelSplicingFailed(FfiConverterString.read(buf))
            16 -> NodeException.ChannelConfigUpdateFailed(FfiConverterString.read(buf))
            17 -> NodeException.PersistenceFailed(FfiConverterString.read(buf))
            18 -> NodeException.FeerateEstimationUpdateFailed(FfiConverterString.read(buf))
            19 -> NodeException.FeerateEstimationUpdateTimeout(FfiConverterString.read(buf))
            20 -> NodeException.WalletOperationFailed(FfiConverterString.read(buf))
            21 -> NodeException.WalletOperationTimeout(FfiConverterString.read(buf))
            22 -> NodeException.OnchainTxSigningFailed(FfiConverterString.read(buf))
            23 -> NodeException.TxSyncFailed(FfiConverterString.read(buf))
            24 -> NodeException.TxSyncTimeout(FfiConverterString.read(buf))
            25 -> NodeException.GossipUpdateFailed(FfiConverterString.read(buf))
            26 -> NodeException.GossipUpdateTimeout(FfiConverterString.read(buf))
            27 -> NodeException.LiquidityRequestFailed(FfiConverterString.read(buf))
            28 -> NodeException.UriParameterParsingFailed(FfiConverterString.read(buf))
            29 -> NodeException.InvalidAddress(FfiConverterString.read(buf))
            30 -> NodeException.InvalidSocketAddress(FfiConverterString.read(buf))
            31 -> NodeException.InvalidPublicKey(FfiConverterString.read(buf))
            32 -> NodeException.InvalidSecretKey(FfiConverterString.read(buf))
            33 -> NodeException.InvalidOfferId(FfiConverterString.read(buf))
            34 -> NodeException.InvalidNodeId(FfiConverterString.read(buf))
            35 -> NodeException.InvalidPaymentId(FfiConverterString.read(buf))
            36 -> NodeException.InvalidPaymentHash(FfiConverterString.read(buf))
            37 -> NodeException.InvalidPaymentPreimage(FfiConverterString.read(buf))
            38 -> NodeException.InvalidPaymentSecret(FfiConverterString.read(buf))
            39 -> NodeException.InvalidAmount(FfiConverterString.read(buf))
            40 -> NodeException.InvalidInvoice(FfiConverterString.read(buf))
            41 -> NodeException.InvalidOffer(FfiConverterString.read(buf))
            42 -> NodeException.InvalidRefund(FfiConverterString.read(buf))
            43 -> NodeException.InvalidChannelId(FfiConverterString.read(buf))
            44 -> NodeException.InvalidNetwork(FfiConverterString.read(buf))
            45 -> NodeException.InvalidUri(FfiConverterString.read(buf))
            46 -> NodeException.InvalidQuantity(FfiConverterString.read(buf))
            47 -> NodeException.InvalidNodeAlias(FfiConverterString.read(buf))
            48 -> NodeException.InvalidDateTime(FfiConverterString.read(buf))
            49 -> NodeException.InvalidFeeRate(FfiConverterString.read(buf))
            50 -> NodeException.DuplicatePayment(FfiConverterString.read(buf))
            51 -> NodeException.UnsupportedCurrency(FfiConverterString.read(buf))
            52 -> NodeException.InsufficientFunds(FfiConverterString.read(buf))
            53 -> NodeException.LiquiditySourceUnavailable(FfiConverterString.read(buf))
            54 -> NodeException.LiquidityFeeTooHigh(FfiConverterString.read(buf))
            55 -> NodeException.InvalidBlindedPaths(FfiConverterString.read(buf))
            56 -> NodeException.AsyncPaymentServicesDisabled(FfiConverterString.read(buf))
            57 -> NodeException.CannotRbfFundingTransaction(FfiConverterString.read(buf))
            58 -> NodeException.TransactionNotFound(FfiConverterString.read(buf))
            59 -> NodeException.TransactionAlreadyConfirmed(FfiConverterString.read(buf))
            60 -> NodeException.NoSpendableOutputs(FfiConverterString.read(buf))
            61 -> NodeException.CoinSelectionFailed(FfiConverterString.read(buf))
            62 -> NodeException.InvalidMnemonic(FfiConverterString.read(buf))
            else -> throw RuntimeException("invalid error enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: NodeException): ULong {
        return 4UL
    }

    override fun write(value: NodeException, buf: ByteBuffer) {
        when (value) {
            is NodeException.AlreadyRunning -> {
                buf.putInt(1)
                Unit
            }
            is NodeException.NotRunning -> {
                buf.putInt(2)
                Unit
            }
            is NodeException.OnchainTxCreationFailed -> {
                buf.putInt(3)
                Unit
            }
            is NodeException.ConnectionFailed -> {
                buf.putInt(4)
                Unit
            }
            is NodeException.InvoiceCreationFailed -> {
                buf.putInt(5)
                Unit
            }
            is NodeException.InvoiceRequestCreationFailed -> {
                buf.putInt(6)
                Unit
            }
            is NodeException.OfferCreationFailed -> {
                buf.putInt(7)
                Unit
            }
            is NodeException.RefundCreationFailed -> {
                buf.putInt(8)
                Unit
            }
            is NodeException.PaymentSendingFailed -> {
                buf.putInt(9)
                Unit
            }
            is NodeException.InvalidCustomTlvs -> {
                buf.putInt(10)
                Unit
            }
            is NodeException.ProbeSendingFailed -> {
                buf.putInt(11)
                Unit
            }
            is NodeException.RouteNotFound -> {
                buf.putInt(12)
                Unit
            }
            is NodeException.ChannelCreationFailed -> {
                buf.putInt(13)
                Unit
            }
            is NodeException.ChannelClosingFailed -> {
                buf.putInt(14)
                Unit
            }
            is NodeException.ChannelSplicingFailed -> {
                buf.putInt(15)
                Unit
            }
            is NodeException.ChannelConfigUpdateFailed -> {
                buf.putInt(16)
                Unit
            }
            is NodeException.PersistenceFailed -> {
                buf.putInt(17)
                Unit
            }
            is NodeException.FeerateEstimationUpdateFailed -> {
                buf.putInt(18)
                Unit
            }
            is NodeException.FeerateEstimationUpdateTimeout -> {
                buf.putInt(19)
                Unit
            }
            is NodeException.WalletOperationFailed -> {
                buf.putInt(20)
                Unit
            }
            is NodeException.WalletOperationTimeout -> {
                buf.putInt(21)
                Unit
            }
            is NodeException.OnchainTxSigningFailed -> {
                buf.putInt(22)
                Unit
            }
            is NodeException.TxSyncFailed -> {
                buf.putInt(23)
                Unit
            }
            is NodeException.TxSyncTimeout -> {
                buf.putInt(24)
                Unit
            }
            is NodeException.GossipUpdateFailed -> {
                buf.putInt(25)
                Unit
            }
            is NodeException.GossipUpdateTimeout -> {
                buf.putInt(26)
                Unit
            }
            is NodeException.LiquidityRequestFailed -> {
                buf.putInt(27)
                Unit
            }
            is NodeException.UriParameterParsingFailed -> {
                buf.putInt(28)
                Unit
            }
            is NodeException.InvalidAddress -> {
                buf.putInt(29)
                Unit
            }
            is NodeException.InvalidSocketAddress -> {
                buf.putInt(30)
                Unit
            }
            is NodeException.InvalidPublicKey -> {
                buf.putInt(31)
                Unit
            }
            is NodeException.InvalidSecretKey -> {
                buf.putInt(32)
                Unit
            }
            is NodeException.InvalidOfferId -> {
                buf.putInt(33)
                Unit
            }
            is NodeException.InvalidNodeId -> {
                buf.putInt(34)
                Unit
            }
            is NodeException.InvalidPaymentId -> {
                buf.putInt(35)
                Unit
            }
            is NodeException.InvalidPaymentHash -> {
                buf.putInt(36)
                Unit
            }
            is NodeException.InvalidPaymentPreimage -> {
                buf.putInt(37)
                Unit
            }
            is NodeException.InvalidPaymentSecret -> {
                buf.putInt(38)
                Unit
            }
            is NodeException.InvalidAmount -> {
                buf.putInt(39)
                Unit
            }
            is NodeException.InvalidInvoice -> {
                buf.putInt(40)
                Unit
            }
            is NodeException.InvalidOffer -> {
                buf.putInt(41)
                Unit
            }
            is NodeException.InvalidRefund -> {
                buf.putInt(42)
                Unit
            }
            is NodeException.InvalidChannelId -> {
                buf.putInt(43)
                Unit
            }
            is NodeException.InvalidNetwork -> {
                buf.putInt(44)
                Unit
            }
            is NodeException.InvalidUri -> {
                buf.putInt(45)
                Unit
            }
            is NodeException.InvalidQuantity -> {
                buf.putInt(46)
                Unit
            }
            is NodeException.InvalidNodeAlias -> {
                buf.putInt(47)
                Unit
            }
            is NodeException.InvalidDateTime -> {
                buf.putInt(48)
                Unit
            }
            is NodeException.InvalidFeeRate -> {
                buf.putInt(49)
                Unit
            }
            is NodeException.DuplicatePayment -> {
                buf.putInt(50)
                Unit
            }
            is NodeException.UnsupportedCurrency -> {
                buf.putInt(51)
                Unit
            }
            is NodeException.InsufficientFunds -> {
                buf.putInt(52)
                Unit
            }
            is NodeException.LiquiditySourceUnavailable -> {
                buf.putInt(53)
                Unit
            }
            is NodeException.LiquidityFeeTooHigh -> {
                buf.putInt(54)
                Unit
            }
            is NodeException.InvalidBlindedPaths -> {
                buf.putInt(55)
                Unit
            }
            is NodeException.AsyncPaymentServicesDisabled -> {
                buf.putInt(56)
                Unit
            }
            is NodeException.CannotRbfFundingTransaction -> {
                buf.putInt(57)
                Unit
            }
            is NodeException.TransactionNotFound -> {
                buf.putInt(58)
                Unit
            }
            is NodeException.TransactionAlreadyConfirmed -> {
                buf.putInt(59)
                Unit
            }
            is NodeException.NoSpendableOutputs -> {
                buf.putInt(60)
                Unit
            }
            is NodeException.CoinSelectionFailed -> {
                buf.putInt(61)
                Unit
            }
            is NodeException.InvalidMnemonic -> {
                buf.putInt(62)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





object FfiConverterTypeOfferAmount : FfiConverterRustBuffer<OfferAmount>{
    override fun read(buf: ByteBuffer): OfferAmount {
        return when(buf.getInt()) {
            1 -> OfferAmount.Bitcoin(
                FfiConverterULong.read(buf),
                )
            2 -> OfferAmount.Currency(
                FfiConverterString.read(buf),
                FfiConverterULong.read(buf),
                )
            else -> throw RuntimeException("invalid enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: OfferAmount) = when(value) {
        is OfferAmount.Bitcoin -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterULong.allocationSize(value.`amountMsats`)
            )
        }
        is OfferAmount.Currency -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterString.allocationSize(value.`iso4217Code`)
                + FfiConverterULong.allocationSize(value.`amount`)
            )
        }
    }

    override fun write(value: OfferAmount, buf: ByteBuffer) {
        when(value) {
            is OfferAmount.Bitcoin -> {
                buf.putInt(1)
                FfiConverterULong.write(value.`amountMsats`, buf)
                Unit
            }
            is OfferAmount.Currency -> {
                buf.putInt(2)
                FfiConverterString.write(value.`iso4217Code`, buf)
                FfiConverterULong.write(value.`amount`, buf)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





object FfiConverterTypePaymentDirection: FfiConverterRustBuffer<PaymentDirection> {
    override fun read(buf: ByteBuffer) = try {
        PaymentDirection.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: PaymentDirection) = 4UL

    override fun write(value: PaymentDirection, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





object FfiConverterTypePaymentFailureReason: FfiConverterRustBuffer<PaymentFailureReason> {
    override fun read(buf: ByteBuffer) = try {
        PaymentFailureReason.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: PaymentFailureReason) = 4UL

    override fun write(value: PaymentFailureReason, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





object FfiConverterTypePaymentKind : FfiConverterRustBuffer<PaymentKind>{
    override fun read(buf: ByteBuffer): PaymentKind {
        return when(buf.getInt()) {
            1 -> PaymentKind.Onchain(
                FfiConverterTypeTxid.read(buf),
                FfiConverterTypeConfirmationStatus.read(buf),
                )
            2 -> PaymentKind.Bolt11(
                FfiConverterTypePaymentHash.read(buf),
                FfiConverterOptionalTypePaymentPreimage.read(buf),
                FfiConverterOptionalTypePaymentSecret.read(buf),
                FfiConverterOptionalString.read(buf),
                FfiConverterOptionalString.read(buf),
                )
            3 -> PaymentKind.Bolt11Jit(
                FfiConverterTypePaymentHash.read(buf),
                FfiConverterOptionalTypePaymentPreimage.read(buf),
                FfiConverterOptionalTypePaymentSecret.read(buf),
                FfiConverterOptionalULong.read(buf),
                FfiConverterTypeLSPFeeLimits.read(buf),
                FfiConverterOptionalString.read(buf),
                FfiConverterOptionalString.read(buf),
                )
            4 -> PaymentKind.Bolt12Offer(
                FfiConverterOptionalTypePaymentHash.read(buf),
                FfiConverterOptionalTypePaymentPreimage.read(buf),
                FfiConverterOptionalTypePaymentSecret.read(buf),
                FfiConverterTypeOfferId.read(buf),
                FfiConverterOptionalTypeUntrustedString.read(buf),
                FfiConverterOptionalULong.read(buf),
                )
            5 -> PaymentKind.Bolt12Refund(
                FfiConverterOptionalTypePaymentHash.read(buf),
                FfiConverterOptionalTypePaymentPreimage.read(buf),
                FfiConverterOptionalTypePaymentSecret.read(buf),
                FfiConverterOptionalTypeUntrustedString.read(buf),
                FfiConverterOptionalULong.read(buf),
                )
            6 -> PaymentKind.Spontaneous(
                FfiConverterTypePaymentHash.read(buf),
                FfiConverterOptionalTypePaymentPreimage.read(buf),
                )
            else -> throw RuntimeException("invalid enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: PaymentKind) = when(value) {
        is PaymentKind.Onchain -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeTxid.allocationSize(value.`txid`)
                + FfiConverterTypeConfirmationStatus.allocationSize(value.`status`)
            )
        }
        is PaymentKind.Bolt11 -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypePaymentHash.allocationSize(value.`hash`)
                + FfiConverterOptionalTypePaymentPreimage.allocationSize(value.`preimage`)
                + FfiConverterOptionalTypePaymentSecret.allocationSize(value.`secret`)
                + FfiConverterOptionalString.allocationSize(value.`description`)
                + FfiConverterOptionalString.allocationSize(value.`bolt11`)
            )
        }
        is PaymentKind.Bolt11Jit -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypePaymentHash.allocationSize(value.`hash`)
                + FfiConverterOptionalTypePaymentPreimage.allocationSize(value.`preimage`)
                + FfiConverterOptionalTypePaymentSecret.allocationSize(value.`secret`)
                + FfiConverterOptionalULong.allocationSize(value.`counterpartySkimmedFeeMsat`)
                + FfiConverterTypeLSPFeeLimits.allocationSize(value.`lspFeeLimits`)
                + FfiConverterOptionalString.allocationSize(value.`description`)
                + FfiConverterOptionalString.allocationSize(value.`bolt11`)
            )
        }
        is PaymentKind.Bolt12Offer -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterOptionalTypePaymentHash.allocationSize(value.`hash`)
                + FfiConverterOptionalTypePaymentPreimage.allocationSize(value.`preimage`)
                + FfiConverterOptionalTypePaymentSecret.allocationSize(value.`secret`)
                + FfiConverterTypeOfferId.allocationSize(value.`offerId`)
                + FfiConverterOptionalTypeUntrustedString.allocationSize(value.`payerNote`)
                + FfiConverterOptionalULong.allocationSize(value.`quantity`)
            )
        }
        is PaymentKind.Bolt12Refund -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterOptionalTypePaymentHash.allocationSize(value.`hash`)
                + FfiConverterOptionalTypePaymentPreimage.allocationSize(value.`preimage`)
                + FfiConverterOptionalTypePaymentSecret.allocationSize(value.`secret`)
                + FfiConverterOptionalTypeUntrustedString.allocationSize(value.`payerNote`)
                + FfiConverterOptionalULong.allocationSize(value.`quantity`)
            )
        }
        is PaymentKind.Spontaneous -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypePaymentHash.allocationSize(value.`hash`)
                + FfiConverterOptionalTypePaymentPreimage.allocationSize(value.`preimage`)
            )
        }
    }

    override fun write(value: PaymentKind, buf: ByteBuffer) {
        when(value) {
            is PaymentKind.Onchain -> {
                buf.putInt(1)
                FfiConverterTypeTxid.write(value.`txid`, buf)
                FfiConverterTypeConfirmationStatus.write(value.`status`, buf)
                Unit
            }
            is PaymentKind.Bolt11 -> {
                buf.putInt(2)
                FfiConverterTypePaymentHash.write(value.`hash`, buf)
                FfiConverterOptionalTypePaymentPreimage.write(value.`preimage`, buf)
                FfiConverterOptionalTypePaymentSecret.write(value.`secret`, buf)
                FfiConverterOptionalString.write(value.`description`, buf)
                FfiConverterOptionalString.write(value.`bolt11`, buf)
                Unit
            }
            is PaymentKind.Bolt11Jit -> {
                buf.putInt(3)
                FfiConverterTypePaymentHash.write(value.`hash`, buf)
                FfiConverterOptionalTypePaymentPreimage.write(value.`preimage`, buf)
                FfiConverterOptionalTypePaymentSecret.write(value.`secret`, buf)
                FfiConverterOptionalULong.write(value.`counterpartySkimmedFeeMsat`, buf)
                FfiConverterTypeLSPFeeLimits.write(value.`lspFeeLimits`, buf)
                FfiConverterOptionalString.write(value.`description`, buf)
                FfiConverterOptionalString.write(value.`bolt11`, buf)
                Unit
            }
            is PaymentKind.Bolt12Offer -> {
                buf.putInt(4)
                FfiConverterOptionalTypePaymentHash.write(value.`hash`, buf)
                FfiConverterOptionalTypePaymentPreimage.write(value.`preimage`, buf)
                FfiConverterOptionalTypePaymentSecret.write(value.`secret`, buf)
                FfiConverterTypeOfferId.write(value.`offerId`, buf)
                FfiConverterOptionalTypeUntrustedString.write(value.`payerNote`, buf)
                FfiConverterOptionalULong.write(value.`quantity`, buf)
                Unit
            }
            is PaymentKind.Bolt12Refund -> {
                buf.putInt(5)
                FfiConverterOptionalTypePaymentHash.write(value.`hash`, buf)
                FfiConverterOptionalTypePaymentPreimage.write(value.`preimage`, buf)
                FfiConverterOptionalTypePaymentSecret.write(value.`secret`, buf)
                FfiConverterOptionalTypeUntrustedString.write(value.`payerNote`, buf)
                FfiConverterOptionalULong.write(value.`quantity`, buf)
                Unit
            }
            is PaymentKind.Spontaneous -> {
                buf.putInt(6)
                FfiConverterTypePaymentHash.write(value.`hash`, buf)
                FfiConverterOptionalTypePaymentPreimage.write(value.`preimage`, buf)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





object FfiConverterTypePaymentStatus: FfiConverterRustBuffer<PaymentStatus> {
    override fun read(buf: ByteBuffer) = try {
        PaymentStatus.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: PaymentStatus) = 4UL

    override fun write(value: PaymentStatus, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}





object FfiConverterTypePendingSweepBalance : FfiConverterRustBuffer<PendingSweepBalance>{
    override fun read(buf: ByteBuffer): PendingSweepBalance {
        return when(buf.getInt()) {
            1 -> PendingSweepBalance.PendingBroadcast(
                FfiConverterOptionalTypeChannelId.read(buf),
                FfiConverterULong.read(buf),
                )
            2 -> PendingSweepBalance.BroadcastAwaitingConfirmation(
                FfiConverterOptionalTypeChannelId.read(buf),
                FfiConverterUInt.read(buf),
                FfiConverterTypeTxid.read(buf),
                FfiConverterULong.read(buf),
                )
            3 -> PendingSweepBalance.AwaitingThresholdConfirmations(
                FfiConverterOptionalTypeChannelId.read(buf),
                FfiConverterTypeTxid.read(buf),
                FfiConverterTypeBlockHash.read(buf),
                FfiConverterUInt.read(buf),
                FfiConverterULong.read(buf),
                )
            else -> throw RuntimeException("invalid enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: PendingSweepBalance) = when(value) {
        is PendingSweepBalance.PendingBroadcast -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterOptionalTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterULong.allocationSize(value.`amountSatoshis`)
            )
        }
        is PendingSweepBalance.BroadcastAwaitingConfirmation -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterOptionalTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterUInt.allocationSize(value.`latestBroadcastHeight`)
                + FfiConverterTypeTxid.allocationSize(value.`latestSpendingTxid`)
                + FfiConverterULong.allocationSize(value.`amountSatoshis`)
            )
        }
        is PendingSweepBalance.AwaitingThresholdConfirmations -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterOptionalTypeChannelId.allocationSize(value.`channelId`)
                + FfiConverterTypeTxid.allocationSize(value.`latestSpendingTxid`)
                + FfiConverterTypeBlockHash.allocationSize(value.`confirmationHash`)
                + FfiConverterUInt.allocationSize(value.`confirmationHeight`)
                + FfiConverterULong.allocationSize(value.`amountSatoshis`)
            )
        }
    }

    override fun write(value: PendingSweepBalance, buf: ByteBuffer) {
        when(value) {
            is PendingSweepBalance.PendingBroadcast -> {
                buf.putInt(1)
                FfiConverterOptionalTypeChannelId.write(value.`channelId`, buf)
                FfiConverterULong.write(value.`amountSatoshis`, buf)
                Unit
            }
            is PendingSweepBalance.BroadcastAwaitingConfirmation -> {
                buf.putInt(2)
                FfiConverterOptionalTypeChannelId.write(value.`channelId`, buf)
                FfiConverterUInt.write(value.`latestBroadcastHeight`, buf)
                FfiConverterTypeTxid.write(value.`latestSpendingTxid`, buf)
                FfiConverterULong.write(value.`amountSatoshis`, buf)
                Unit
            }
            is PendingSweepBalance.AwaitingThresholdConfirmations -> {
                buf.putInt(3)
                FfiConverterOptionalTypeChannelId.write(value.`channelId`, buf)
                FfiConverterTypeTxid.write(value.`latestSpendingTxid`, buf)
                FfiConverterTypeBlockHash.write(value.`confirmationHash`, buf)
                FfiConverterUInt.write(value.`confirmationHeight`, buf)
                FfiConverterULong.write(value.`amountSatoshis`, buf)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





object FfiConverterTypeQrPaymentResult : FfiConverterRustBuffer<QrPaymentResult>{
    override fun read(buf: ByteBuffer): QrPaymentResult {
        return when(buf.getInt()) {
            1 -> QrPaymentResult.Onchain(
                FfiConverterTypeTxid.read(buf),
                )
            2 -> QrPaymentResult.Bolt11(
                FfiConverterTypePaymentId.read(buf),
                )
            3 -> QrPaymentResult.Bolt12(
                FfiConverterTypePaymentId.read(buf),
                )
            else -> throw RuntimeException("invalid enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: QrPaymentResult) = when(value) {
        is QrPaymentResult.Onchain -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypeTxid.allocationSize(value.`txid`)
            )
        }
        is QrPaymentResult.Bolt11 -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypePaymentId.allocationSize(value.`paymentId`)
            )
        }
        is QrPaymentResult.Bolt12 -> {
            // Add the size for the Int that specifies the variant plus the size needed for all fields
            (
                4UL
                + FfiConverterTypePaymentId.allocationSize(value.`paymentId`)
            )
        }
    }

    override fun write(value: QrPaymentResult, buf: ByteBuffer) {
        when(value) {
            is QrPaymentResult.Onchain -> {
                buf.putInt(1)
                FfiConverterTypeTxid.write(value.`txid`, buf)
                Unit
            }
            is QrPaymentResult.Bolt11 -> {
                buf.putInt(2)
                FfiConverterTypePaymentId.write(value.`paymentId`, buf)
                Unit
            }
            is QrPaymentResult.Bolt12 -> {
                buf.putInt(3)
                FfiConverterTypePaymentId.write(value.`paymentId`, buf)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





object FfiConverterTypeSyncType: FfiConverterRustBuffer<SyncType> {
    override fun read(buf: ByteBuffer) = try {
        SyncType.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: SyncType) = 4UL

    override fun write(value: SyncType, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}




object VssHeaderProviderExceptionErrorHandler : UniffiRustCallStatusErrorHandler<VssHeaderProviderException> {
    override fun lift(errorBuf: RustBufferByValue): VssHeaderProviderException = FfiConverterTypeVssHeaderProviderError.lift(errorBuf)
}

object FfiConverterTypeVssHeaderProviderError : FfiConverterRustBuffer<VssHeaderProviderException> {
    override fun read(buf: ByteBuffer): VssHeaderProviderException {
        return when (buf.getInt()) {
            1 -> VssHeaderProviderException.InvalidData(FfiConverterString.read(buf))
            2 -> VssHeaderProviderException.RequestException(FfiConverterString.read(buf))
            3 -> VssHeaderProviderException.AuthorizationException(FfiConverterString.read(buf))
            4 -> VssHeaderProviderException.InternalException(FfiConverterString.read(buf))
            else -> throw RuntimeException("invalid error enum value, something is very wrong!!")
        }
    }

    override fun allocationSize(value: VssHeaderProviderException): ULong {
        return 4UL
    }

    override fun write(value: VssHeaderProviderException, buf: ByteBuffer) {
        when (value) {
            is VssHeaderProviderException.InvalidData -> {
                buf.putInt(1)
                Unit
            }
            is VssHeaderProviderException.RequestException -> {
                buf.putInt(2)
                Unit
            }
            is VssHeaderProviderException.AuthorizationException -> {
                buf.putInt(3)
                Unit
            }
            is VssHeaderProviderException.InternalException -> {
                buf.putInt(4)
                Unit
            }
        }.let { /* this makes the `when` an expression, which ensures it is exhaustive */ }
    }
}





object FfiConverterTypeWordCount: FfiConverterRustBuffer<WordCount> {
    override fun read(buf: ByteBuffer) = try {
        WordCount.entries[buf.getInt() - 1]
    } catch (e: IndexOutOfBoundsException) {
        throw RuntimeException("invalid enum value, something is very wrong!!", e)
    }

    override fun allocationSize(value: WordCount) = 4UL

    override fun write(value: WordCount, buf: ByteBuffer) {
        buf.putInt(value.ordinal + 1)
    }
}




object FfiConverterOptionalUShort: FfiConverterRustBuffer<kotlin.UShort?> {
    override fun read(buf: ByteBuffer): kotlin.UShort? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterUShort.read(buf)
    }

    override fun allocationSize(value: kotlin.UShort?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterUShort.allocationSize(value)
        }
    }

    override fun write(value: kotlin.UShort?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterUShort.write(value, buf)
        }
    }
}




object FfiConverterOptionalUInt: FfiConverterRustBuffer<kotlin.UInt?> {
    override fun read(buf: ByteBuffer): kotlin.UInt? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterUInt.read(buf)
    }

    override fun allocationSize(value: kotlin.UInt?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterUInt.allocationSize(value)
        }
    }

    override fun write(value: kotlin.UInt?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterUInt.write(value, buf)
        }
    }
}




object FfiConverterOptionalULong: FfiConverterRustBuffer<kotlin.ULong?> {
    override fun read(buf: ByteBuffer): kotlin.ULong? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterULong.read(buf)
    }

    override fun allocationSize(value: kotlin.ULong?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterULong.allocationSize(value)
        }
    }

    override fun write(value: kotlin.ULong?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterULong.write(value, buf)
        }
    }
}




object FfiConverterOptionalBoolean: FfiConverterRustBuffer<kotlin.Boolean?> {
    override fun read(buf: ByteBuffer): kotlin.Boolean? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterBoolean.read(buf)
    }

    override fun allocationSize(value: kotlin.Boolean?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterBoolean.allocationSize(value)
        }
    }

    override fun write(value: kotlin.Boolean?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterBoolean.write(value, buf)
        }
    }
}




object FfiConverterOptionalString: FfiConverterRustBuffer<kotlin.String?> {
    override fun read(buf: ByteBuffer): kotlin.String? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterString.read(buf)
    }

    override fun allocationSize(value: kotlin.String?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterString.allocationSize(value)
        }
    }

    override fun write(value: kotlin.String?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterString.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeFeeRate: FfiConverterRustBuffer<FeeRate?> {
    override fun read(buf: ByteBuffer): FeeRate? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeFeeRate.read(buf)
    }

    override fun allocationSize(value: FeeRate?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeFeeRate.allocationSize(value)
        }
    }

    override fun write(value: FeeRate?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeFeeRate.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeAnchorChannelsConfig: FfiConverterRustBuffer<AnchorChannelsConfig?> {
    override fun read(buf: ByteBuffer): AnchorChannelsConfig? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeAnchorChannelsConfig.read(buf)
    }

    override fun allocationSize(value: AnchorChannelsConfig?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeAnchorChannelsConfig.allocationSize(value)
        }
    }

    override fun write(value: AnchorChannelsConfig?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeAnchorChannelsConfig.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeBackgroundSyncConfig: FfiConverterRustBuffer<BackgroundSyncConfig?> {
    override fun read(buf: ByteBuffer): BackgroundSyncConfig? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeBackgroundSyncConfig.read(buf)
    }

    override fun allocationSize(value: BackgroundSyncConfig?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeBackgroundSyncConfig.allocationSize(value)
        }
    }

    override fun write(value: BackgroundSyncConfig?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeBackgroundSyncConfig.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeChannelConfig: FfiConverterRustBuffer<ChannelConfig?> {
    override fun read(buf: ByteBuffer): ChannelConfig? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeChannelConfig.read(buf)
    }

    override fun allocationSize(value: ChannelConfig?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeChannelConfig.allocationSize(value)
        }
    }

    override fun write(value: ChannelConfig?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeChannelConfig.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeChannelInfo: FfiConverterRustBuffer<ChannelInfo?> {
    override fun read(buf: ByteBuffer): ChannelInfo? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeChannelInfo.read(buf)
    }

    override fun allocationSize(value: ChannelInfo?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeChannelInfo.allocationSize(value)
        }
    }

    override fun write(value: ChannelInfo?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeChannelInfo.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeChannelUpdateInfo: FfiConverterRustBuffer<ChannelUpdateInfo?> {
    override fun read(buf: ByteBuffer): ChannelUpdateInfo? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeChannelUpdateInfo.read(buf)
    }

    override fun allocationSize(value: ChannelUpdateInfo?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeChannelUpdateInfo.allocationSize(value)
        }
    }

    override fun write(value: ChannelUpdateInfo?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeChannelUpdateInfo.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeElectrumSyncConfig: FfiConverterRustBuffer<ElectrumSyncConfig?> {
    override fun read(buf: ByteBuffer): ElectrumSyncConfig? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeElectrumSyncConfig.read(buf)
    }

    override fun allocationSize(value: ElectrumSyncConfig?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeElectrumSyncConfig.allocationSize(value)
        }
    }

    override fun write(value: ElectrumSyncConfig?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeElectrumSyncConfig.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeEsploraSyncConfig: FfiConverterRustBuffer<EsploraSyncConfig?> {
    override fun read(buf: ByteBuffer): EsploraSyncConfig? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeEsploraSyncConfig.read(buf)
    }

    override fun allocationSize(value: EsploraSyncConfig?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeEsploraSyncConfig.allocationSize(value)
        }
    }

    override fun write(value: EsploraSyncConfig?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeEsploraSyncConfig.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeLSPS1Bolt11PaymentInfo: FfiConverterRustBuffer<Lsps1Bolt11PaymentInfo?> {
    override fun read(buf: ByteBuffer): Lsps1Bolt11PaymentInfo? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeLSPS1Bolt11PaymentInfo.read(buf)
    }

    override fun allocationSize(value: Lsps1Bolt11PaymentInfo?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeLSPS1Bolt11PaymentInfo.allocationSize(value)
        }
    }

    override fun write(value: Lsps1Bolt11PaymentInfo?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeLSPS1Bolt11PaymentInfo.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeLSPS1ChannelInfo: FfiConverterRustBuffer<Lsps1ChannelInfo?> {
    override fun read(buf: ByteBuffer): Lsps1ChannelInfo? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeLSPS1ChannelInfo.read(buf)
    }

    override fun allocationSize(value: Lsps1ChannelInfo?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeLSPS1ChannelInfo.allocationSize(value)
        }
    }

    override fun write(value: Lsps1ChannelInfo?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeLSPS1ChannelInfo.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeLSPS1OnchainPaymentInfo: FfiConverterRustBuffer<Lsps1OnchainPaymentInfo?> {
    override fun read(buf: ByteBuffer): Lsps1OnchainPaymentInfo? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeLSPS1OnchainPaymentInfo.read(buf)
    }

    override fun allocationSize(value: Lsps1OnchainPaymentInfo?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeLSPS1OnchainPaymentInfo.allocationSize(value)
        }
    }

    override fun write(value: Lsps1OnchainPaymentInfo?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeLSPS1OnchainPaymentInfo.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeNodeAnnouncementInfo: FfiConverterRustBuffer<NodeAnnouncementInfo?> {
    override fun read(buf: ByteBuffer): NodeAnnouncementInfo? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeNodeAnnouncementInfo.read(buf)
    }

    override fun allocationSize(value: NodeAnnouncementInfo?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeNodeAnnouncementInfo.allocationSize(value)
        }
    }

    override fun write(value: NodeAnnouncementInfo?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeNodeAnnouncementInfo.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeNodeInfo: FfiConverterRustBuffer<NodeInfo?> {
    override fun read(buf: ByteBuffer): NodeInfo? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeNodeInfo.read(buf)
    }

    override fun allocationSize(value: NodeInfo?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeNodeInfo.allocationSize(value)
        }
    }

    override fun write(value: NodeInfo?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeNodeInfo.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeOutPoint: FfiConverterRustBuffer<OutPoint?> {
    override fun read(buf: ByteBuffer): OutPoint? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeOutPoint.read(buf)
    }

    override fun allocationSize(value: OutPoint?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeOutPoint.allocationSize(value)
        }
    }

    override fun write(value: OutPoint?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeOutPoint.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypePaymentDetails: FfiConverterRustBuffer<PaymentDetails?> {
    override fun read(buf: ByteBuffer): PaymentDetails? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaymentDetails.read(buf)
    }

    override fun allocationSize(value: PaymentDetails?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaymentDetails.allocationSize(value)
        }
    }

    override fun write(value: PaymentDetails?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaymentDetails.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeRouteParametersConfig: FfiConverterRustBuffer<RouteParametersConfig?> {
    override fun read(buf: ByteBuffer): RouteParametersConfig? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeRouteParametersConfig.read(buf)
    }

    override fun allocationSize(value: RouteParametersConfig?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeRouteParametersConfig.allocationSize(value)
        }
    }

    override fun write(value: RouteParametersConfig?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeRouteParametersConfig.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeTransactionDetails: FfiConverterRustBuffer<TransactionDetails?> {
    override fun read(buf: ByteBuffer): TransactionDetails? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeTransactionDetails.read(buf)
    }

    override fun allocationSize(value: TransactionDetails?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeTransactionDetails.allocationSize(value)
        }
    }

    override fun write(value: TransactionDetails?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeTransactionDetails.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeAsyncPaymentsRole: FfiConverterRustBuffer<AsyncPaymentsRole?> {
    override fun read(buf: ByteBuffer): AsyncPaymentsRole? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeAsyncPaymentsRole.read(buf)
    }

    override fun allocationSize(value: AsyncPaymentsRole?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeAsyncPaymentsRole.allocationSize(value)
        }
    }

    override fun write(value: AsyncPaymentsRole?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeAsyncPaymentsRole.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeClosureReason: FfiConverterRustBuffer<ClosureReason?> {
    override fun read(buf: ByteBuffer): ClosureReason? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeClosureReason.read(buf)
    }

    override fun allocationSize(value: ClosureReason?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeClosureReason.allocationSize(value)
        }
    }

    override fun write(value: ClosureReason?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeClosureReason.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeEvent: FfiConverterRustBuffer<Event?> {
    override fun read(buf: ByteBuffer): Event? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeEvent.read(buf)
    }

    override fun allocationSize(value: Event?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeEvent.allocationSize(value)
        }
    }

    override fun write(value: Event?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeEvent.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeLogLevel: FfiConverterRustBuffer<LogLevel?> {
    override fun read(buf: ByteBuffer): LogLevel? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeLogLevel.read(buf)
    }

    override fun allocationSize(value: LogLevel?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeLogLevel.allocationSize(value)
        }
    }

    override fun write(value: LogLevel?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeLogLevel.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeNetwork: FfiConverterRustBuffer<Network?> {
    override fun read(buf: ByteBuffer): Network? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeNetwork.read(buf)
    }

    override fun allocationSize(value: Network?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeNetwork.allocationSize(value)
        }
    }

    override fun write(value: Network?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeNetwork.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeOfferAmount: FfiConverterRustBuffer<OfferAmount?> {
    override fun read(buf: ByteBuffer): OfferAmount? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeOfferAmount.read(buf)
    }

    override fun allocationSize(value: OfferAmount?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeOfferAmount.allocationSize(value)
        }
    }

    override fun write(value: OfferAmount?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeOfferAmount.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypePaymentFailureReason: FfiConverterRustBuffer<PaymentFailureReason?> {
    override fun read(buf: ByteBuffer): PaymentFailureReason? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaymentFailureReason.read(buf)
    }

    override fun allocationSize(value: PaymentFailureReason?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaymentFailureReason.allocationSize(value)
        }
    }

    override fun write(value: PaymentFailureReason?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaymentFailureReason.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeWordCount: FfiConverterRustBuffer<WordCount?> {
    override fun read(buf: ByteBuffer): WordCount? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeWordCount.read(buf)
    }

    override fun allocationSize(value: WordCount?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeWordCount.allocationSize(value)
        }
    }

    override fun write(value: WordCount?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeWordCount.write(value, buf)
        }
    }
}




object FfiConverterOptionalSequenceUByte: FfiConverterRustBuffer<List<kotlin.UByte>?> {
    override fun read(buf: ByteBuffer): List<kotlin.UByte>? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterSequenceUByte.read(buf)
    }

    override fun allocationSize(value: List<kotlin.UByte>?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterSequenceUByte.allocationSize(value)
        }
    }

    override fun write(value: List<kotlin.UByte>?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterSequenceUByte.write(value, buf)
        }
    }
}




object FfiConverterOptionalSequenceTypeSpendableUtxo: FfiConverterRustBuffer<List<SpendableUtxo>?> {
    override fun read(buf: ByteBuffer): List<SpendableUtxo>? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterSequenceTypeSpendableUtxo.read(buf)
    }

    override fun allocationSize(value: List<SpendableUtxo>?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterSequenceTypeSpendableUtxo.allocationSize(value)
        }
    }

    override fun write(value: List<SpendableUtxo>?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterSequenceTypeSpendableUtxo.write(value, buf)
        }
    }
}




object FfiConverterOptionalSequenceSequenceUByte: FfiConverterRustBuffer<List<List<kotlin.UByte>>?> {
    override fun read(buf: ByteBuffer): List<List<kotlin.UByte>>? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterSequenceSequenceUByte.read(buf)
    }

    override fun allocationSize(value: List<List<kotlin.UByte>>?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterSequenceSequenceUByte.allocationSize(value)
        }
    }

    override fun write(value: List<List<kotlin.UByte>>?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterSequenceSequenceUByte.write(value, buf)
        }
    }
}




object FfiConverterOptionalSequenceTypeSocketAddress: FfiConverterRustBuffer<List<SocketAddress>?> {
    override fun read(buf: ByteBuffer): List<SocketAddress>? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterSequenceTypeSocketAddress.read(buf)
    }

    override fun allocationSize(value: List<SocketAddress>?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterSequenceTypeSocketAddress.allocationSize(value)
        }
    }

    override fun write(value: List<SocketAddress>?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterSequenceTypeSocketAddress.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeAddress: FfiConverterRustBuffer<Address?> {
    override fun read(buf: ByteBuffer): Address? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeAddress.read(buf)
    }

    override fun allocationSize(value: Address?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeAddress.allocationSize(value)
        }
    }

    override fun write(value: Address?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeAddress.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeChannelId: FfiConverterRustBuffer<ChannelId?> {
    override fun read(buf: ByteBuffer): ChannelId? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeChannelId.read(buf)
    }

    override fun allocationSize(value: ChannelId?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeChannelId.allocationSize(value)
        }
    }

    override fun write(value: ChannelId?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeChannelId.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeNodeAlias: FfiConverterRustBuffer<NodeAlias?> {
    override fun read(buf: ByteBuffer): NodeAlias? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeNodeAlias.read(buf)
    }

    override fun allocationSize(value: NodeAlias?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeNodeAlias.allocationSize(value)
        }
    }

    override fun write(value: NodeAlias?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeNodeAlias.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypePaymentHash: FfiConverterRustBuffer<PaymentHash?> {
    override fun read(buf: ByteBuffer): PaymentHash? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaymentHash.read(buf)
    }

    override fun allocationSize(value: PaymentHash?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaymentHash.allocationSize(value)
        }
    }

    override fun write(value: PaymentHash?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaymentHash.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypePaymentId: FfiConverterRustBuffer<PaymentId?> {
    override fun read(buf: ByteBuffer): PaymentId? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaymentId.read(buf)
    }

    override fun allocationSize(value: PaymentId?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaymentId.allocationSize(value)
        }
    }

    override fun write(value: PaymentId?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaymentId.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypePaymentPreimage: FfiConverterRustBuffer<PaymentPreimage?> {
    override fun read(buf: ByteBuffer): PaymentPreimage? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaymentPreimage.read(buf)
    }

    override fun allocationSize(value: PaymentPreimage?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaymentPreimage.allocationSize(value)
        }
    }

    override fun write(value: PaymentPreimage?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaymentPreimage.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypePaymentSecret: FfiConverterRustBuffer<PaymentSecret?> {
    override fun read(buf: ByteBuffer): PaymentSecret? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePaymentSecret.read(buf)
    }

    override fun allocationSize(value: PaymentSecret?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePaymentSecret.allocationSize(value)
        }
    }

    override fun write(value: PaymentSecret?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePaymentSecret.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypePublicKey: FfiConverterRustBuffer<PublicKey?> {
    override fun read(buf: ByteBuffer): PublicKey? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypePublicKey.read(buf)
    }

    override fun allocationSize(value: PublicKey?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypePublicKey.allocationSize(value)
        }
    }

    override fun write(value: PublicKey?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypePublicKey.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeUntrustedString: FfiConverterRustBuffer<UntrustedString?> {
    override fun read(buf: ByteBuffer): UntrustedString? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeUntrustedString.read(buf)
    }

    override fun allocationSize(value: UntrustedString?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeUntrustedString.allocationSize(value)
        }
    }

    override fun write(value: UntrustedString?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeUntrustedString.write(value, buf)
        }
    }
}




object FfiConverterOptionalTypeUserChannelId: FfiConverterRustBuffer<UserChannelId?> {
    override fun read(buf: ByteBuffer): UserChannelId? {
        if (buf.get().toInt() == 0) {
            return null
        }
        return FfiConverterTypeUserChannelId.read(buf)
    }

    override fun allocationSize(value: UserChannelId?): ULong {
        if (value == null) {
            return 1UL
        } else {
            return 1UL + FfiConverterTypeUserChannelId.allocationSize(value)
        }
    }

    override fun write(value: UserChannelId?, buf: ByteBuffer) {
        if (value == null) {
            buf.put(0)
        } else {
            buf.put(1)
            FfiConverterTypeUserChannelId.write(value, buf)
        }
    }
}




object FfiConverterSequenceUByte: FfiConverterRustBuffer<List<kotlin.UByte>> {
    override fun read(buf: ByteBuffer): List<kotlin.UByte> {
        val len = buf.getInt()
        return List<kotlin.UByte>(len) {
            FfiConverterUByte.read(buf)
        }
    }

    override fun allocationSize(value: List<kotlin.UByte>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterUByte.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<kotlin.UByte>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterUByte.write(it, buf)
        }
    }
}




object FfiConverterSequenceULong: FfiConverterRustBuffer<List<kotlin.ULong>> {
    override fun read(buf: ByteBuffer): List<kotlin.ULong> {
        val len = buf.getInt()
        return List<kotlin.ULong>(len) {
            FfiConverterULong.read(buf)
        }
    }

    override fun allocationSize(value: List<kotlin.ULong>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterULong.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<kotlin.ULong>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterULong.write(it, buf)
        }
    }
}




object FfiConverterSequenceString: FfiConverterRustBuffer<List<kotlin.String>> {
    override fun read(buf: ByteBuffer): List<kotlin.String> {
        val len = buf.getInt()
        return List<kotlin.String>(len) {
            FfiConverterString.read(buf)
        }
    }

    override fun allocationSize(value: List<kotlin.String>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterString.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<kotlin.String>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterString.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypeChannelDetails: FfiConverterRustBuffer<List<ChannelDetails>> {
    override fun read(buf: ByteBuffer): List<ChannelDetails> {
        val len = buf.getInt()
        return List<ChannelDetails>(len) {
            FfiConverterTypeChannelDetails.read(buf)
        }
    }

    override fun allocationSize(value: List<ChannelDetails>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeChannelDetails.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<ChannelDetails>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeChannelDetails.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypeCustomTlvRecord: FfiConverterRustBuffer<List<CustomTlvRecord>> {
    override fun read(buf: ByteBuffer): List<CustomTlvRecord> {
        val len = buf.getInt()
        return List<CustomTlvRecord>(len) {
            FfiConverterTypeCustomTlvRecord.read(buf)
        }
    }

    override fun allocationSize(value: List<CustomTlvRecord>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeCustomTlvRecord.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<CustomTlvRecord>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeCustomTlvRecord.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypePaymentDetails: FfiConverterRustBuffer<List<PaymentDetails>> {
    override fun read(buf: ByteBuffer): List<PaymentDetails> {
        val len = buf.getInt()
        return List<PaymentDetails>(len) {
            FfiConverterTypePaymentDetails.read(buf)
        }
    }

    override fun allocationSize(value: List<PaymentDetails>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePaymentDetails.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PaymentDetails>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePaymentDetails.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypePeerDetails: FfiConverterRustBuffer<List<PeerDetails>> {
    override fun read(buf: ByteBuffer): List<PeerDetails> {
        val len = buf.getInt()
        return List<PeerDetails>(len) {
            FfiConverterTypePeerDetails.read(buf)
        }
    }

    override fun allocationSize(value: List<PeerDetails>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePeerDetails.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PeerDetails>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePeerDetails.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypeRouteHintHop: FfiConverterRustBuffer<List<RouteHintHop>> {
    override fun read(buf: ByteBuffer): List<RouteHintHop> {
        val len = buf.getInt()
        return List<RouteHintHop>(len) {
            FfiConverterTypeRouteHintHop.read(buf)
        }
    }

    override fun allocationSize(value: List<RouteHintHop>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeRouteHintHop.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<RouteHintHop>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeRouteHintHop.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypeSpendableUtxo: FfiConverterRustBuffer<List<SpendableUtxo>> {
    override fun read(buf: ByteBuffer): List<SpendableUtxo> {
        val len = buf.getInt()
        return List<SpendableUtxo>(len) {
            FfiConverterTypeSpendableUtxo.read(buf)
        }
    }

    override fun allocationSize(value: List<SpendableUtxo>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeSpendableUtxo.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<SpendableUtxo>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeSpendableUtxo.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypeTxInput: FfiConverterRustBuffer<List<TxInput>> {
    override fun read(buf: ByteBuffer): List<TxInput> {
        val len = buf.getInt()
        return List<TxInput>(len) {
            FfiConverterTypeTxInput.read(buf)
        }
    }

    override fun allocationSize(value: List<TxInput>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeTxInput.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<TxInput>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeTxInput.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypeTxOutput: FfiConverterRustBuffer<List<TxOutput>> {
    override fun read(buf: ByteBuffer): List<TxOutput> {
        val len = buf.getInt()
        return List<TxOutput>(len) {
            FfiConverterTypeTxOutput.read(buf)
        }
    }

    override fun allocationSize(value: List<TxOutput>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeTxOutput.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<TxOutput>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeTxOutput.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypeLightningBalance: FfiConverterRustBuffer<List<LightningBalance>> {
    override fun read(buf: ByteBuffer): List<LightningBalance> {
        val len = buf.getInt()
        return List<LightningBalance>(len) {
            FfiConverterTypeLightningBalance.read(buf)
        }
    }

    override fun allocationSize(value: List<LightningBalance>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeLightningBalance.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<LightningBalance>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeLightningBalance.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypeNetwork: FfiConverterRustBuffer<List<Network>> {
    override fun read(buf: ByteBuffer): List<Network> {
        val len = buf.getInt()
        return List<Network>(len) {
            FfiConverterTypeNetwork.read(buf)
        }
    }

    override fun allocationSize(value: List<Network>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeNetwork.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<Network>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeNetwork.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypePendingSweepBalance: FfiConverterRustBuffer<List<PendingSweepBalance>> {
    override fun read(buf: ByteBuffer): List<PendingSweepBalance> {
        val len = buf.getInt()
        return List<PendingSweepBalance>(len) {
            FfiConverterTypePendingSweepBalance.read(buf)
        }
    }

    override fun allocationSize(value: List<PendingSweepBalance>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePendingSweepBalance.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PendingSweepBalance>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePendingSweepBalance.write(it, buf)
        }
    }
}




object FfiConverterSequenceSequenceUByte: FfiConverterRustBuffer<List<List<kotlin.UByte>>> {
    override fun read(buf: ByteBuffer): List<List<kotlin.UByte>> {
        val len = buf.getInt()
        return List<List<kotlin.UByte>>(len) {
            FfiConverterSequenceUByte.read(buf)
        }
    }

    override fun allocationSize(value: List<List<kotlin.UByte>>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterSequenceUByte.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<List<kotlin.UByte>>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterSequenceUByte.write(it, buf)
        }
    }
}




object FfiConverterSequenceSequenceTypeRouteHintHop: FfiConverterRustBuffer<List<List<RouteHintHop>>> {
    override fun read(buf: ByteBuffer): List<List<RouteHintHop>> {
        val len = buf.getInt()
        return List<List<RouteHintHop>>(len) {
            FfiConverterSequenceTypeRouteHintHop.read(buf)
        }
    }

    override fun allocationSize(value: List<List<RouteHintHop>>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterSequenceTypeRouteHintHop.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<List<RouteHintHop>>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterSequenceTypeRouteHintHop.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypeAddress: FfiConverterRustBuffer<List<Address>> {
    override fun read(buf: ByteBuffer): List<Address> {
        val len = buf.getInt()
        return List<Address>(len) {
            FfiConverterTypeAddress.read(buf)
        }
    }

    override fun allocationSize(value: List<Address>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeAddress.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<Address>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeAddress.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypeNodeId: FfiConverterRustBuffer<List<NodeId>> {
    override fun read(buf: ByteBuffer): List<NodeId> {
        val len = buf.getInt()
        return List<NodeId>(len) {
            FfiConverterTypeNodeId.read(buf)
        }
    }

    override fun allocationSize(value: List<NodeId>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeNodeId.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<NodeId>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeNodeId.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypePublicKey: FfiConverterRustBuffer<List<PublicKey>> {
    override fun read(buf: ByteBuffer): List<PublicKey> {
        val len = buf.getInt()
        return List<PublicKey>(len) {
            FfiConverterTypePublicKey.read(buf)
        }
    }

    override fun allocationSize(value: List<PublicKey>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypePublicKey.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<PublicKey>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypePublicKey.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypeSocketAddress: FfiConverterRustBuffer<List<SocketAddress>> {
    override fun read(buf: ByteBuffer): List<SocketAddress> {
        val len = buf.getInt()
        return List<SocketAddress>(len) {
            FfiConverterTypeSocketAddress.read(buf)
        }
    }

    override fun allocationSize(value: List<SocketAddress>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeSocketAddress.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<SocketAddress>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeSocketAddress.write(it, buf)
        }
    }
}




object FfiConverterSequenceTypeTxid: FfiConverterRustBuffer<List<Txid>> {
    override fun read(buf: ByteBuffer): List<Txid> {
        val len = buf.getInt()
        return List<Txid>(len) {
            FfiConverterTypeTxid.read(buf)
        }
    }

    override fun allocationSize(value: List<Txid>): ULong {
        val sizeForLength = 4UL
        val sizeForItems = value.sumOf { FfiConverterTypeTxid.allocationSize(it) }
        return sizeForLength + sizeForItems
    }

    override fun write(value: List<Txid>, buf: ByteBuffer) {
        buf.putInt(value.size)
        value.iterator().forEach {
            FfiConverterTypeTxid.write(it, buf)
        }
    }
}



object FfiConverterMapStringString: FfiConverterRustBuffer<Map<kotlin.String, kotlin.String>> {
    override fun read(buf: ByteBuffer): Map<kotlin.String, kotlin.String> {
        val len = buf.getInt()
        return buildMap<kotlin.String, kotlin.String>(len) {
            repeat(len) {
                val k = FfiConverterString.read(buf)
                val v = FfiConverterString.read(buf)
                this[k] = v
            }
        }
    }

    override fun allocationSize(value: Map<kotlin.String, kotlin.String>): ULong {
        val spaceForMapSize = 4UL
        val spaceForChildren = value.entries.sumOf { (k, v) ->
            FfiConverterString.allocationSize(k) +
            FfiConverterString.allocationSize(v)
        }
        return spaceForMapSize + spaceForChildren
    }

    override fun write(value: Map<kotlin.String, kotlin.String>, buf: ByteBuffer) {
        buf.putInt(value.size)
        // The parens on `(k, v)` here ensure we're calling the right method,
        // which is important for compatibility with older android devices.
        // Ref https://blog.danlew.net/2017/03/16/kotlin-puzzler-whose-line-is-it-anyways/
        value.forEach { (k, v) ->
            FfiConverterString.write(k, buf)
            FfiConverterString.write(v, buf)
        }
    }
}




typealias FfiConverterTypeAddress = FfiConverterString




typealias FfiConverterTypeBlockHash = FfiConverterString




typealias FfiConverterTypeChannelId = FfiConverterString




typealias FfiConverterTypeLSPS1OrderId = FfiConverterString




typealias FfiConverterTypeLSPSDateTime = FfiConverterString




typealias FfiConverterTypeMnemonic = FfiConverterString




typealias FfiConverterTypeNodeAlias = FfiConverterString




typealias FfiConverterTypeNodeId = FfiConverterString




typealias FfiConverterTypeOfferId = FfiConverterString




typealias FfiConverterTypePaymentHash = FfiConverterString




typealias FfiConverterTypePaymentId = FfiConverterString




typealias FfiConverterTypePaymentPreimage = FfiConverterString




typealias FfiConverterTypePaymentSecret = FfiConverterString




typealias FfiConverterTypePublicKey = FfiConverterString




typealias FfiConverterTypeSocketAddress = FfiConverterString




typealias FfiConverterTypeTxid = FfiConverterString




typealias FfiConverterTypeUntrustedString = FfiConverterString




typealias FfiConverterTypeUserChannelId = FfiConverterString













fun `defaultConfig`(): Config {
    return FfiConverterTypeConfig.lift(uniffiRustCall { uniffiRustCallStatus ->
        UniffiLib.INSTANCE.uniffi_ldk_node_fn_func_default_config(
            uniffiRustCallStatus,
        )
    })
}

@Throws(NodeException::class)
fun `deriveNodeSecretFromMnemonic`(`mnemonic`: kotlin.String, `passphrase`: kotlin.String?): List<kotlin.UByte> {
    return FfiConverterSequenceUByte.lift(uniffiRustCallWithError(NodeExceptionErrorHandler) { uniffiRustCallStatus ->
        UniffiLib.INSTANCE.uniffi_ldk_node_fn_func_derive_node_secret_from_mnemonic(
            FfiConverterString.lower(`mnemonic`),
            FfiConverterOptionalString.lower(`passphrase`),
            uniffiRustCallStatus,
        )
    })
}

fun `generateEntropyMnemonic`(`wordCount`: WordCount?): Mnemonic {
    return FfiConverterTypeMnemonic.lift(uniffiRustCall { uniffiRustCallStatus ->
        UniffiLib.INSTANCE.uniffi_ldk_node_fn_func_generate_entropy_mnemonic(
            FfiConverterOptionalTypeWordCount.lower(`wordCount`),
            uniffiRustCallStatus,
        )
    })
}


// Async support

internal const val UNIFFI_RUST_FUTURE_POLL_READY = 0.toByte()
internal const val UNIFFI_RUST_FUTURE_POLL_MAYBE_READY = 1.toByte()

internal val uniffiContinuationHandleMap = UniffiHandleMap<CancellableContinuation<Byte>>()

// FFI type for Rust future continuations
internal suspend fun<T, F, E: kotlin.Exception> uniffiRustCallAsync(
    rustFuture: Long,
    pollFunc: (Long, UniffiRustFutureContinuationCallback, Long) -> Unit,
    completeFunc: (Long, UniffiRustCallStatus) -> F,
    freeFunc: (Long) -> Unit,
    cancelFunc: (Long) -> Unit,
    liftFunc: (F) -> T,
    errorHandler: UniffiRustCallStatusErrorHandler<E>
): T {
    return withContext(Dispatchers.IO) {
        try {
            do {
                val pollResult = suspendCancellableCoroutine<Byte> { continuation ->
                    val handle = uniffiContinuationHandleMap.insert(continuation)
                    continuation.invokeOnCancellation {
                        cancelFunc(rustFuture)
                    }
                    pollFunc(
                        rustFuture,
                        uniffiRustFutureContinuationCallbackCallback,
                        handle
                    )
                }
            } while (pollResult != UNIFFI_RUST_FUTURE_POLL_READY);

            return@withContext liftFunc(
                uniffiRustCallWithError(errorHandler) { status -> completeFunc(rustFuture, status) }
            )
        } finally {
            freeFunc(rustFuture)
        }
    }
}

object uniffiRustFutureContinuationCallbackCallback: UniffiRustFutureContinuationCallback {
    override fun callback(data: Long, pollResult: Byte) {
        uniffiContinuationHandleMap.remove(data).resume(pollResult)
    }
}