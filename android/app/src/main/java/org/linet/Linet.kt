package org.linet

import android.content.Context
import android.provider.Settings
import android.util.Log
import kotlinx.coroutines.flow.MutableStateFlow
import kotlinx.coroutines.flow.StateFlow
import kotlinx.coroutines.flow.asStateFlow
import uniffi.linet_ffi.ConnState
import uniffi.linet_ffi.EventListener
import uniffi.linet_ffi.LinetClient
import uniffi.linet_ffi.PeerInfo
import uniffi.linet_ffi.TransferInfo
import java.io.File

sealed interface Status {
    data object NotPaired : Status
    data class Offline(val peer: PeerInfo, val reason: String?) : Status
    data class Connected(val peer: PeerInfo) : Status
}

/** Единственный клиент на процесс. Живёт, пока жив процесс; сервис лишь не даёт процессу умереть. */
object Linet {
    private const val TAG = "Linet"

    @Volatile
    private var instance: LinetClient? = null
    private val _status = MutableStateFlow<Status>(Status.NotPaired)
    val status: StateFlow<Status> = _status.asStateFlow()

    fun client(context: Context): LinetClient = instance ?: synchronized(this) {
        instance ?: create(context.applicationContext).also { instance = it }
    }

    private fun create(context: Context): LinetClient {
        val dir = File(context.filesDir, "linet").apply { mkdirs() }
        // Не кеш: при ошибке сохранения файл должен пережить очистку кеша системой.
        val inbox = File(context.filesDir, "incoming").apply { mkdirs() }
        val client = LinetClient(dir.path, inbox.path, deviceName(context), Listener(context))
        _status.value = client.paired()?.let { Status.Offline(it, null) } ?: Status.NotPaired
        return client
    }

    private fun deviceName(context: Context): String =
        Settings.Global.getString(context.contentResolver, Settings.Global.DEVICE_NAME)
            ?: android.os.Build.MODEL

    /** Вызывается из потока Rust. */
    private class Listener(private val context: Context) : EventListener {
        override fun onState(state: ConnState) {
            Log.i(TAG, "state: $state")
            _status.value = when (state) {
                is ConnState.Connected -> Status.Connected(state.peer)
                is ConnState.Disconnected ->
                    instance?.paired()?.let { Status.Offline(it, state.reason) } ?: Status.NotPaired
                is ConnState.Unpaired -> Status.NotPaired
            }
            if (state is ConnState.Unpaired) LinetService.stop(context)
            if (state is ConnState.Connected) NotifyListener.onConnected()
            if (state !is ConnState.Connected) ScreenShare.disconnected(context)
        }

        override fun onNotificationAction(key: String, action: UInt, reply: String?) {
            NotifyListener.onAction(key, action, reply)
        }

        override fun onNotificationDismiss(key: String) {
            NotifyListener.onDismiss(key)
        }

        override fun onClipboard(text: String) {
            ClipboardSync.apply(context, text)
        }

        override fun onClipboardImage(mime: String, data: ByteArray) {
            // Запись файла — не на потоке Rust.
            Thread { ClipboardSync.applyImage(context, mime, data) }.start()
        }

        override fun onHandoff(url: String, title: String) {
            Handoff.show(context, url, title)
        }

        override fun onScreenShare(requestId: ULong, enable: Boolean) {
            val error = ScreenShare.set(context, enable)
            instance?.answerScreenShare(requestId, error == null, error.orEmpty())
        }

        override fun onUnlockRequest(requestId: ULong, host: String) {
            Unlock.onRequest(context, requestId, host)
        }

        override fun onTransfer(transfer: TransferInfo) = Transfers.onUpdate(context, transfer)
    }
}
