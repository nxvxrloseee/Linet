package org.linet

import android.app.KeyguardManager
import android.app.Activity
import android.app.Notification
import android.app.NotificationChannel
import android.app.NotificationManager
import android.app.PendingIntent
import android.content.BroadcastReceiver
import android.content.Context
import android.content.Intent
import android.graphics.drawable.Icon
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PowerManager

/**
 * Разблокировка ноутбука телефоном.
 * Телефон в руке и разблокирован → согласие сразу, без касаний (если включено).
 * Иначе — уведомление, кнопка которого требует отпечаток/PIN телефона.
 */
object Unlock {
    private const val CHANNEL_ASK = "unlock"
    private const val CHANNEL_INFO = "unlock_info"
    private const val NOTIFICATION_ID = 3
    private const val DECISION_MS = 30_000L
    private const val PREFS = "linet"
    private const val KEY_AUTO = "auto_unlock"
    const val EXTRA_ID = "id"
    const val EXTRA_APPROVE = "approve"

    private val main = Handler(Looper.getMainLooper())
    private val pending = HashSet<ULong>()

    fun isAuto(context: Context): Boolean =
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).getBoolean(KEY_AUTO, true)

    fun setAuto(context: Context, enabled: Boolean) {
        context.getSharedPreferences(PREFS, Context.MODE_PRIVATE).edit().putBoolean(KEY_AUTO, enabled).apply()
    }

    /** Вызывается из потока Rust. */
    fun onRequest(context: Context, id: ULong, host: String) = main.post {
        val keyguard = context.getSystemService(KeyguardManager::class.java)
        val power = context.getSystemService(PowerManager::class.java)
        if (isAuto(context) && power.isInteractive && !keyguard.isDeviceLocked) {
            answer(context, id, true, "телефон разблокирован")
            info(context, "$host разблокирован")
            return@post
        }
        pending.add(id)
        ask(context, id, host)
        main.postDelayed({ resolve(context, id, false, "время вышло") }, DECISION_MS)
    }

    fun resolve(context: Context, id: ULong, approved: Boolean, reason: String) {
        val allowed = takeApproval(context, id, approved) ?: return
        context.getSystemService(NotificationManager::class.java).cancel(NOTIFICATION_ID)
        answer(context, id, allowed, if (approved && !allowed) "телефон заблокирован" else reason)
    }

    /** Одноразовое решение с проверкой блокировки в момент подтверждения. */
    internal fun takeApproval(context: Context, id: ULong, approved: Boolean): Boolean? {
        if (!pending.remove(id)) return null
        return approved && !context.getSystemService(KeyguardManager::class.java).isDeviceLocked
    }

    private fun answer(context: Context, id: ULong, approved: Boolean, reason: String) =
        Linet.client(context).answerUnlock(id, approved, reason)

    private fun channels(nm: NotificationManager) {
        if (nm.getNotificationChannel(CHANNEL_ASK) != null) return
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_ASK, "Запрос разблокировки", NotificationManager.IMPORTANCE_HIGH),
        )
        nm.createNotificationChannel(
            NotificationChannel(CHANNEL_INFO, "Ноутбук разблокирован", NotificationManager.IMPORTANCE_LOW),
        )
    }

    private fun ask(context: Context, id: ULong, host: String) {
        val nm = context.getSystemService(NotificationManager::class.java)
        channels(nm)
        fun action(approve: Boolean, title: String): Notification.Action {
            val needsActivity = approve && Build.VERSION.SDK_INT < 31
            val target = if (needsActivity) UnlockActivity::class.java else UnlockReceiver::class.java
            val intent = Intent(context, target)
                .putExtra(EXTRA_ID, id.toLong())
                .putExtra(EXTRA_APPROVE, approve)
            val flags = PendingIntent.FLAG_IMMUTABLE or PendingIntent.FLAG_UPDATE_CURRENT
            val pi = if (needsActivity) {
                PendingIntent.getActivity(context, 1, intent, flags)
            } else {
                PendingIntent.getBroadcast(context, if (approve) 1 else 0, intent, flags)
            }
            val builder = Notification.Action.Builder(Icon.createWithResource(context, R.drawable.ic_linet), title, pi)
            // Кнопка «Разблокировать» на экране блокировки телефона требует его разблокировки.
            if (approve && Build.VERSION.SDK_INT >= 31) builder.setAuthenticationRequired(true)
            return builder.build()
        }
        val notification = Notification.Builder(context, CHANNEL_ASK)
            .setSmallIcon(R.drawable.ic_linet)
            .setContentTitle("Разблокировать $host?")
            .setContentText("Ноутбук ждёт подтверждения")
            .setCategory(Notification.CATEGORY_CALL)
            .setTimeoutAfter(DECISION_MS)
            .addAction(action(true, "Разблокировать"))
            .addAction(action(false, "Отклонить"))
            .build()
        nm.notify(NOTIFICATION_ID, notification)
    }

    /** Ненавязчивое напоминание, что ноутбук разблокировался: чтобы это не прошло незаметно. */
    private fun info(context: Context, text: String) {
        val nm = context.getSystemService(NotificationManager::class.java)
        channels(nm)
        val notification = Notification.Builder(context, CHANNEL_INFO)
            .setSmallIcon(R.drawable.ic_linet)
            .setContentTitle(text)
            .setTimeoutAfter(5_000)
            .build()
        nm.notify(NOTIFICATION_ID, notification)
    }
}

/** Android 10–11: сначала системный PIN/отпечаток, затем согласие ноутбуку. */
class UnlockActivity : Activity() {
    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        setShowWhenLocked(true)
        val id = intent.getLongExtra(Unlock.EXTRA_ID, -1).toULong()
        val keyguard = getSystemService(KeyguardManager::class.java)
        if (!keyguard.isDeviceLocked) {
            Unlock.resolve(this, id, true, "подтверждено на телефоне")
            finish()
            return
        }
        keyguard.requestDismissKeyguard(this, object : KeyguardManager.KeyguardDismissCallback() {
            override fun onDismissSucceeded() = complete(true)
            override fun onDismissCancelled() = complete(false)
            override fun onDismissError() = complete(false)

            private fun complete(approved: Boolean) {
                Unlock.resolve(this@UnlockActivity, id, approved, if (approved) "подтверждено на телефоне" else "разблокировка отменена")
                finish()
            }
        })
    }
}

class UnlockReceiver : BroadcastReceiver() {
    override fun onReceive(context: Context, intent: Intent) {
        val id = intent.getLongExtra(Unlock.EXTRA_ID, -1).toULong()
        val approve = intent.getBooleanExtra(Unlock.EXTRA_APPROVE, false)
        Unlock.resolve(context, id, approve, if (approve) "подтверждено на телефоне" else "отклонено на телефоне")
    }
}
