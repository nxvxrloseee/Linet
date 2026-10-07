package org.linet

import android.app.ActivityOptions
import android.app.Notification
import android.app.NotificationManager
import android.app.PendingIntent
import android.app.RemoteInput
import android.content.ComponentName
import android.content.Context
import android.content.Intent
import android.graphics.Bitmap
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.provider.Settings
import android.service.notification.NotificationListenerService
import android.service.notification.StatusBarNotification
import android.util.Log
import androidx.core.app.NotificationManagerCompat
import androidx.core.graphics.drawable.toBitmap
import uniffi.linet_ffi.NotificationAction
import uniffi.linet_ffi.NotificationInfo
import java.io.ByteArrayOutputStream

/**
 * Пересылает уведомления телефона на ноутбук.
 * Сервис держит сама система, поэтому батарею он не тратит; работа идёт только
 * в момент появления уведомления и только при активном соединении.
 */
class NotifyListener : NotificationListenerService() {
    private val actions = HashMap<String, List<Notification.Action>>()
    /** Хеш последнего отправленного содержимого: многие приложения перепубликуют одно и то же. */
    private val lastSent = HashMap<String, Int>()
    private val removedOrder = ArrayDeque<String>()
    private val iconsSent = HashSet<String>()
    private val labels = HashMap<String, String>()

    override fun onListenerConnected() {
        instance = this
    }

    override fun onListenerDisconnected() {
        if (instance === this) instance = null
    }

    override fun onNotificationPosted(sbn: StatusBarNotification, rankingMap: RankingMap) {
        if (Linet.status.value !is Status.Connected || sbn.packageName == packageName) return
        if (NotifyFilter.isMuted(this, sbn.packageName)) return
        val n = sbn.notification
        val skipFlags = Notification.FLAG_ONGOING_EVENT or Notification.FLAG_FOREGROUND_SERVICE or
            Notification.FLAG_GROUP_SUMMARY
        if (n.flags and skipFlags != 0) return
        val ranking = Ranking()
        if (rankingMap.getRanking(sbn.key, ranking) && ranking.importance < NotificationManager.IMPORTANCE_DEFAULT) {
            return
        }

        val extras = n.extras
        val title = extras.getCharSequence(Notification.EXTRA_TITLE)?.toString().orEmpty()
        val text = (extras.getCharSequence(Notification.EXTRA_BIG_TEXT) ?: extras.getCharSequence(Notification.EXTRA_TEXT))
            ?.toString().orEmpty()
        if (title.isBlank() && text.isBlank()) return
        val hash = "$title\u0000$text".hashCode()
        if (lastSent[sbn.key] == hash) return
        lastSent[sbn.key] = hash

        NotifyFilter.remember(this, sbn.packageName, label(sbn.packageName))
        val acts = n.actions?.toList().orEmpty()
        actions[sbn.key] = acts
        val buttons = acts.mapIndexed { i, a ->
            NotificationAction(i.toUInt(), a.title?.toString().orEmpty(), !a.remoteInputs.isNullOrEmpty())
        }
        val icon = if (iconsSent.add(sbn.packageName)) appIcon(sbn.packageName) else null
        Linet.client(this).postNotification(
            NotificationInfo(sbn.key, sbn.packageName, label(sbn.packageName), title, text, icon, buttons, sbn.isClearable),
        )
    }

    override fun onNotificationRemoved(sbn: StatusBarNotification) {
        // Кнопки не забываем сразу: ответ с ноутбука может прийти уже после того,
        // как уведомление убрали (PendingIntent при этом остаётся рабочим).
        removedOrder.addLast(sbn.key)
        while (removedOrder.size > KEEP_REMOVED) actions.remove(removedOrder.removeFirst())
        if (lastSent.remove(sbn.key) != null && Linet.status.value is Status.Connected) {
            Linet.client(this).removeNotification(sbn.key)
        }
    }

    private fun onReconnected() {
        // Ноутбук мог перезапуститься и забыть иконки.
        iconsSent.clear()
        lastSent.clear()
    }

    private fun invoke(key: String, index: Int, reply: String?) {
        val action = actions[key]?.getOrNull(index)
        Log.i(TAG, "action $index on $key (reply=${reply != null}, known=${action != null})")
        if (action == null) return
        val inputs = action.remoteInputs
        val fillIn = Intent()
        if (reply != null && !inputs.isNullOrEmpty()) {
            val results = Bundle().apply { inputs.forEach { putCharSequence(it.resultKey, reply) } }
            RemoteInput.addResultsToIntent(inputs, fillIn, results)
        }
        val options = if (Build.VERSION.SDK_INT >= 34) {
            ActivityOptions.makeBasic()
                .setPendingIntentBackgroundActivityStartMode(ActivityOptions.MODE_BACKGROUND_ACTIVITY_START_ALLOWED)
                .toBundle()
        } else {
            null
        }
        try {
            action.actionIntent.send(this, 0, fillIn, null, null, null, options)
            Log.i(TAG, "action sent: ${action.title}")
        } catch (e: PendingIntent.CanceledException) {
            Log.w(TAG, "action $index of $key", e)
        }
    }

    private fun label(pkg: String): String = labels.getOrPut(pkg) {
        runCatching { packageManager.getApplicationLabel(packageManager.getApplicationInfo(pkg, 0)).toString() }
            .getOrDefault(pkg)
    }

    private fun appIcon(pkg: String): ByteArray? = runCatching {
        val bitmap = packageManager.getApplicationIcon(pkg).toBitmap(ICON_SIZE, ICON_SIZE)
        ByteArrayOutputStream().also { bitmap.compress(Bitmap.CompressFormat.PNG, 100, it) }.toByteArray()
    }.getOrNull()

    companion object {
        private const val TAG = "LinetNotify"
        private const val ICON_SIZE = 96
        private const val KEEP_REMOVED = 32

        @Volatile
        private var instance: NotifyListener? = null
        private val handler = Handler(Looper.getMainLooper())

        fun isEnabled(context: Context): Boolean =
            context.packageName in NotificationManagerCompat.getEnabledListenerPackages(context)

        fun openSettings(context: Context) {
            val intent = if (Build.VERSION.SDK_INT >= 30) {
                Intent(Settings.ACTION_NOTIFICATION_LISTENER_DETAIL_SETTINGS).putExtra(
                    Settings.EXTRA_NOTIFICATION_LISTENER_COMPONENT_NAME,
                    ComponentName(context, NotifyListener::class.java).flattenToString(),
                )
            } else {
                Intent(Settings.ACTION_NOTIFICATION_LISTENER_SETTINGS)
            }
            context.startActivity(intent)
        }

        // Вызовы из потока Rust переносим на главный поток, где работает сервис.
        fun onAction(key: String, action: UInt, reply: String?) =
            handler.post { instance?.invoke(key, action.toInt(), reply) }

        fun onDismiss(key: String) = handler.post { instance?.cancelNotification(key) }

        fun onConnected() = handler.post { instance?.onReconnected() }
    }
}
