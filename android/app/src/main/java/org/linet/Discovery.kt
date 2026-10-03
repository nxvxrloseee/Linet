package org.linet

import android.content.Context
import android.net.nsd.NsdManager
import android.net.nsd.NsdServiceInfo
import android.os.Handler
import android.os.Looper
import android.util.Log
import java.net.Inet4Address

/**
 * Короткий mDNS-поиск ноутбука, если его адрес сменился.
 * Поиск держит Wi-Fi в режиме приёма multicast, поэтому он ограничен по времени
 * и запускается только по событию, а не постоянно.
 */
class Discovery(context: Context) {
    private val nsd = context.getSystemService(NsdManager::class.java)
    private val main = Handler(Looper.getMainLooper())
    private var active: NsdManager.DiscoveryListener? = null

    fun findOnce(peerId: String, onFound: (String) -> Unit) {
        if (active != null) return
        val listener = object : NsdManager.DiscoveryListener {
            override fun onServiceFound(info: NsdServiceInfo) = resolve(info, peerId, onFound)
            override fun onDiscoveryStarted(serviceType: String) {}
            override fun onDiscoveryStopped(serviceType: String) {}
            override fun onServiceLost(info: NsdServiceInfo) {}
            override fun onStartDiscoveryFailed(serviceType: String, errorCode: Int) {
                Log.w(TAG, "discovery failed: $errorCode")
                active = null
            }
            override fun onStopDiscoveryFailed(serviceType: String, errorCode: Int) {}
        }
        active = listener
        nsd.discoverServices(SERVICE_TYPE, NsdManager.PROTOCOL_DNS_SD, listener)
        main.postDelayed(::stop, TIMEOUT_MS)
    }

    @Suppress("DEPRECATION") // registerServiceInfoCallback есть только с Android 14
    private fun resolve(info: NsdServiceInfo, peerId: String, onFound: (String) -> Unit) {
        nsd.resolveService(info, object : NsdManager.ResolveListener {
            override fun onResolveFailed(info: NsdServiceInfo, errorCode: Int) {}
            override fun onServiceResolved(info: NsdServiceInfo) {
                val id = info.attributes["id"]?.decodeToString()
                val host = info.host
                if (id != peerId || host !is Inet4Address) return
                Log.i(TAG, "found ${host.hostAddress}:${info.port}")
                runCatching { onFound("${host.hostAddress}:${info.port}") }
                main.post(::stop)
            }
        })
    }

    fun stop() {
        main.removeCallbacksAndMessages(null)
        active?.let { runCatching { nsd.stopServiceDiscovery(it) } }
        active = null
    }

    companion object {
        private const val TAG = "LinetDiscovery"
        private const val SERVICE_TYPE = "_linet._udp"
        private const val TIMEOUT_MS = 15_000L
    }
}
