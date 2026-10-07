package org.linet

import android.app.Activity
import android.app.PendingIntent
import android.content.ClipData
import android.content.ClipboardManager
import android.content.Context
import android.content.Intent
import android.net.Uri
import android.os.Build
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import android.os.PersistableBundle
import android.os.SystemClock
import android.service.quicksettings.Tile
import android.service.quicksettings.TileService
import android.widget.Toast
import androidx.core.content.FileProvider
import java.io.File

/**
 * Буфер обмена.
 * Ноутбук → телефон: автоматически, писать в буфер Android разрешает из фона.
 * Телефон → ноутбук: читать буфер в фоне Android 10+ запрещает, поэтому только по
 * действию пользователя — плитка в шторке, «Поделиться» или кнопка в приложении.
 */
object ClipboardSync {
    private val main = Handler(Looper.getMainLooper())

    /** Сколько после записи очистка с ноутбука ещё касается нашего текста. */
    private const val CLEAR_WINDOW_MS = 2 * 60 * 1000L
    private var lastSetAt = 0L
    /** ClipDescription.EXTRA_IS_SENSITIVE (API 33); на старых версиях ключ просто игнорируется. */
    private const val EXTRA_IS_SENSITIVE = "android.content.extra.IS_SENSITIVE"

    /** Вызывается из потока Rust. Пустой текст — на ноутбуке буфер очистили. */
    fun apply(context: Context, text: String) = main.post {
        val clipboard = context.getSystemService(ClipboardManager::class.java)
        if (text.isEmpty()) {
            // Очищаем, только если в буфере, скорее всего, ещё наш текст: читать буфер
            // в фоне нельзя, поэтому ориентируемся на время.
            if (SystemClock.elapsedRealtime() - lastSetAt < CLEAR_WINDOW_MS) clipboard.clearPrimaryClip()
            return@post
        }
        val clip = ClipData.newPlainText("Linet", text)
        // Похоже на пароль — клавиатура не покажет превью (будут точки) и не сохранит его в истории.
        if (looksLikePassword(text)) {
            clip.description.extras = PersistableBundle().apply { putBoolean(EXTRA_IS_SENSITIVE, true) }
        }
        clipboard.setPrimaryClip(clip)
        lastSetAt = SystemClock.elapsedRealtime()
    }

    /**
     * Одно «слово» 8–64 символа, где есть хотя бы три вида символов из четырёх:
     * строчные, заглавные, цифры, спецсимволы (кроме -_. — они обычны в именах файлов).
     * Ссылки и адреса почты исключаем. Эвристика: иногда ошибётся в обе стороны.
     */
    fun looksLikePassword(text: String): Boolean {
        if (text.length !in 8..64 || text.any { it.isWhitespace() }) return false
        if ("://" in text || EMAIL.matches(text)) return false
        val kinds = listOf(
            text.any { it.isLowerCase() },
            text.any { it.isUpperCase() },
            text.any { it.isDigit() },
            text.any { !it.isLetterOrDigit() && it !in "-_." },
        ).count { it }
        return kinds >= 3
    }

    private val EMAIL = Regex("[^@]+@[^@]+\\.[a-zA-Z]{2,}")

    /** Картинка с ноутбука: файл в кэше, в буфер — ссылка на него через FileProvider. */
    fun applyImage(context: Context, mime: String, data: ByteArray) {
        val ext = when (mime) {
            "image/jpeg" -> "jpg"
            "image/webp" -> "webp"
            "image/gif" -> "gif"
            else -> "png"
        }
        val dir = File(context.cacheDir, "clip").apply { mkdirs() }
        // Прошлые картинки больше не нужны: в буфере может лежать только одна.
        dir.listFiles()?.forEach { it.delete() }
        val file = File(dir, "linet-${System.currentTimeMillis()}.$ext").apply { writeBytes(data) }
        val uri = FileProvider.getUriForFile(context, "org.linet.clip", file)
        main.post {
            val clipboard = context.getSystemService(ClipboardManager::class.java)
            clipboard.setPrimaryClip(ClipData.newUri(context.contentResolver, "Linet", uri))
            lastSetAt = SystemClock.elapsedRealtime()
        }
    }

    /** Читать буфер можно только из окна в фокусе. */
    fun sendCurrent(activity: Activity) {
        val clip = activity.getSystemService(ClipboardManager::class.java).primaryClip
        val item = clip?.takeIf { it.itemCount > 0 }?.getItemAt(0)
        val uri = item?.uri
        val mime = uri?.let { activity.contentResolver.getType(it) }
        if (uri != null && mime != null && mime.startsWith("image/")) {
            sendImage(activity, uri, mime)
            return
        }
        send(activity, item?.coerceToText(activity)?.toString())
    }

    private fun sendImage(activity: Activity, uri: Uri, mime: String) {
        val data = runCatching {
            activity.contentResolver.openInputStream(uri)?.use { input ->
                // Читаем не больше лимита: огромную картинку всё равно не отправить.
                val out = java.io.ByteArrayOutputStream()
                val buf = ByteArray(64 * 1024)
                while (out.size() <= MAX_IMAGE) {
                    val n = input.read(buf)
                    if (n < 0) break
                    out.write(buf, 0, n)
                }
                out.toByteArray().takeIf { it.size <= MAX_IMAGE }
            }
        }.getOrNull()
        val message = when {
            data == null -> "Картинка слишком большая или недоступна"
            !Linet.client(activity).sendClipboardImage(mime, data) -> "Ноутбук не подключён"
            else -> "Картинка скопирована на ноутбук"
        }
        Toast.makeText(activity, message, Toast.LENGTH_SHORT).show()
    }

    /** Должно совпадать с MAX_CLIPBOARD_IMAGE в linet-core. */
    private const val MAX_IMAGE = 12 shl 20

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
