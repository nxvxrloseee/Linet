package org.linet

import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.Service
import android.content.ClipData
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.content.IntentFilter
import android.content.pm.ServiceInfo
import android.net.ConnectivityManager
import android.net.Network
import android.net.NetworkCapabilities
import android.net.NetworkRequest
import android.net.Uri
import android.os.IBinder
import android.util.Log
import androidx.core.app.ServiceCompat
import androidx.core.content.ContextCompat
import kotlinx.coroutines.CoroutineScope
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.SupervisorJob
import kotlinx.coroutines.cancel
import kotlinx.coroutines.flow.distinctUntilChanged
import kotlinx.coroutines.flow.first
import kotlinx.coroutines.withTimeoutOrNull
import kotlinx.coroutines.launch
import kotlinx.coroutines.sync.Mutex
import kotlinx.coroutines.sync.withLock

/**
 * Держит процесс живым, пока есть сопряжённый ноутбук.
 *
 * Экономия батареи:
 * - никаких WakeLock, AlarmManager и периодических задач;
 * - соединение держит Rust-ядро, сам телефон keepalive не шлёт;
 * - переподключение запускается событиями (появился Wi-Fi, включился экран),
 *   а не опросом;
 * - mDNS-поиск — только короткими сеансами, когда ноутбук не найден по старому адресу.
 */
class LinetService : Service() {
    private val scope = CoroutineScope(SupervisorJob() + Dispatchers.Main.immediate)
    private lateinit var connectivity: ConnectivityManager
    private var discovery: Discovery? = null
    private val sending = Mutex()

    private val wifiCallback = object : ConnectivityManager.NetworkCallback() {
        override fun onAvailable(network: Network) = reconnect()
    }

    private val screenOn = object : BroadcastReceiver() {
        override fun onReceive(context: Context, intent: Intent) {
            if (Linet.status.value is Status.Offline) reconnect()
        }
    }

    override fun onCreate() {
        super.onCreate()
        createChannel()
        // startForeground обязателен даже перед немедленной остановкой, иначе Android уронит процесс.
        ServiceCompat.startForeground(
            this, NOTIFICATION_ID, notification(Linet.status.value),
            ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE,
        )
        if (Linet.client(this).paired() == null) {
            stopSelf()
            return
        }

        connectivity = getSystemService(ConnectivityManager::class.java)
        val wifi = NetworkRequest.Builder().addTransportType(NetworkCapabilities.TRANSPORT_WIFI).build()
        connectivity.registerNetworkCallback(wifi, wifiCallback)
        ContextCompat.registerReceiver(
            this, screenOn, IntentFilter(Intent.ACTION_SCREEN_ON), ContextCompat.RECEIVER_NOT_EXPORTED,
        )
        discovery = Discovery(this)

        scope.launch {
            Linet.status.distinctUntilChanged { a, b -> a::class == b::class }.collect { status ->
                getSystemService(NotificationManager::class.java).notify(NOTIFICATION_ID, notification(status))
                if (status is Status.Connected) discovery?.stop()
            }
        }
    }

    private fun reconnect() {
        val client = Linet.client(this)
        if (client.isConnected()) return
        client.kick()
        val peer = client.paired() ?: return
        discovery?.findOnce(peer.id) { addr -> client.addAddress(addr) }
    }

    override fun onStartCommand(intent: Intent?, flags: Int, startId: Int): Int {
        if (intent?.action == ACTION_SEND_FILES) {
            // Каждый startForegroundService требует startForeground, даже если сервис уже запущен.
            ServiceCompat.startForeground(
                this, NOTIFICATION_ID, notification(Linet.status.value),
                ServiceInfo.FOREGROUND_SERVICE_TYPE_CONNECTED_DEVICE,
            )
            val clip = intent.clipData
            val uris = (0 until (clip?.itemCount ?: 0)).mapNotNull { clip!!.getItemAt(it).uri }
            scope.launch(Dispatchers.IO) {
                // По одному файлу: так прогресс понятнее и меньше пиковая нагрузка.
                sending.withLock {
                    val connected = withTimeoutOrNull(CONNECT_WAIT_MS) {
                        Linet.client(this@LinetService).kick()
                        Linet.status.first { it is Status.Connected }
                    }
                    if (connected == null) {
                        Log.w(TAG, "нет соединения, отправка отменена")
                        return@withLock
                    }
                    for (uri in uris) {
                        runCatching { Transfers.send(this@LinetService, uri) }
                            .onFailure { Log.w(TAG, "send $uri", it) }
                    }
                }
            }
        }
        return START_STICKY
    }

    override fun onDestroy() {
        scope.cancel()
        discovery?.stop()
        if (::connectivity.isInitialized) {
            connectivity.unregisterNetworkCallback(wifiCallback)
            unregisterReceiver(screenOn)
        }
        super.onDestroy()
    }

    override fun onBind(intent: Intent?): IBinder? = null

    private fun createChannel() {
        val channel = NotificationChannel(CHANNEL_ID, "Подключение", NotificationManager.IMPORTANCE_MIN).apply {
            description = "Постоянное уведомление, без него Android закроет Linet в фоне"
            setShowBadge(false)
        }
        getSystemService(NotificationManager::class.java).createNotificationChannel(channel)
    }

    private fun notification(status: Status): Notification {
        val text = when (status) {
            is Status.Connected -> "Подключено к ${status.peer.name}"
            is Status.Offline -> "${status.peer.name} не в сети"
            Status.NotPaired -> "Нет сопряжённых устройств"
        }
        val open = PendingIntent.getActivity(
            this, 0, Intent(this, MainActivity::class.java), PendingIntent.FLAG_IMMUTABLE,
        )
        return Notification.Builder(this, CHANNEL_ID)
            .setSmallIcon(R.drawable.ic_linet)
            .setContentTitle(text)
            .setContentIntent(open)
            .setOngoing(true)
            .setShowWhen(false)
            .build()
    }

    companion object {
        private const val TAG = "LinetService"
        private const val ACTION_SEND_FILES = "org.linet.SEND_FILES"
        private const val CONNECT_WAIT_MS = 15_000L
        private const val CHANNEL_ID = "connection"
        private const val NOTIFICATION_ID = 1

        fun start(context: Context) {
            ContextCompat.startForegroundService(context, Intent(context, LinetService::class.java))
        }

        /** URI передаются через clipData вместе с правом на чтение. */
        fun sendFiles(context: Context, uris: List<Uri>) {
            if (uris.isEmpty()) return
            val clip = ClipData.newRawUri("files", uris.first())
            uris.drop(1).forEach { clip.addItem(ClipData.Item(it)) }
            val intent = Intent(context, LinetService::class.java)
                .setAction(ACTION_SEND_FILES)
                .addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION)
            intent.clipData = clip
            ContextCompat.startForegroundService(context, intent)
        }

        fun stop(context: Context) {
            context.stopService(Intent(context, LinetService::class.java))
        }
    }
}
