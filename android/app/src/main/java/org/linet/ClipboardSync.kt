package org.linet

import android.app.Activity
import android.app.PendingIntent
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import android.widget.Toast

/**
 * Буфер обмена.
 * Ноутбук → телефон: автоматически, писать в буфер Android разрешает из фона.
 * Телефон → ноутбук: читать буфер в фоне Android 10+ запрещает, поэтому только по
 * действию пользователя — плитка в шторке, «Поделиться» или кнопка в приложении.
 */
object ClipboardSync {
    private val main = Handler(Looper.getMainLooper())

    /** Вызывается из потока Rust. */
    fun apply(context: Context, text: String) = main.post {
        context.getSystemService(ClipboardManager::class.java).setPrimaryClip(ClipData.newPlainText("Linet", text))
    }

    /** Читать буфер можно только из окна в фокусе. */
    fun sendCurrent(activity: Activity) {
        val clip = activity.getSystemService(ClipboardManager::class.java).primaryClip
        val text = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)?.coerceToText(activity)?.toString()
        send(activity, text)
    }

    fun send(context: Context, text: String?) {
        val message = when {
            text.isNullOrEmpty() -> "Буфер обмена пуст"
            !Linet.client(context).sendClipboard(text) -> "Ноутбук не подключён"
            else -> "Скопировано на ноутбук"
        }
        Toast.makeText(context, message, Toast.LENGTH_SHORT).show()
    }
}

/** Прозрачное окно на долю секунды: получить фокус, прочитать буфер, закрыться. */
class ClipboardActivity : Activity() {
    private var sent = false

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        // Если окно так и не получит фокус (например, его перекрыли), не висим.
        Handler(Looper.getMainLooper()).postDelayed(::finish, 2000)
    }

    override fun onWindowFocusChanged(hasFocus: Boolean) {
        super.onWindowFocusChanged(hasFocus)
        if (hasFocus && !sent) {
            sent = true
            ClipboardSync.sendCurrent(this)
            finish()
        }
    }
}

/** Плитка «Буфер → ноутбук» в шторке. */
class ClipboardTile : TileService() {
    override fun onStartListening() {
        qsTile?.apply {
            state = if (Linet.status.value is Status.Connected) Tile.STATE_INACTIVE else Tile.STATE_UNAVAILABLE
            updateTile()
        }
    }

    override fun onClick() {
        if (isLocked) unlockAndRun(::launch) else launch()
    }

    private fun launch() {
        val intent = Intent(this, ClipboardActivity::class.java).addFlags(Intent.FLAG_ACTIVITY_NEW_TASK)
        if (Build.VERSION.SDK_INT >= 34) {
            startActivityAndCollapse(PendingIntent.getActivity(this, 0, intent, PendingIntent.FLAG_IMMUTABLE))
        } else {
            @Suppress("DEPRECATION")
            startActivityAndCollapse(intent)
        }
    }
}
