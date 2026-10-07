package org.linet

import android.content.Context

/**
 * Какие приложения не пересылать на ноутбук (банки, Госуслуги и т. п.).
 * Список «виденных» приложений копится сам: в настройках показываются только те,
 * от кого уже приходили уведомления.
 */
object NotifyFilter {
    private const val PREFS = "notify_filter"
    private const val KEY_MUTED = "muted"
    private const val KEY_SEEN = "seen"

    private fun prefs(context: Context) = context.getSharedPreferences(PREFS, Context.MODE_PRIVATE)

    @Volatile
    private var muted: Set<String>? = null

    fun isMuted(context: Context, pkg: String): Boolean =
        pkg in (muted ?: prefs(context).getStringSet(KEY_MUTED, emptySet()).orEmpty().also { muted = it })

    fun setMuted(context: Context, pkg: String, mute: Boolean) {
        val now = prefs(context).getStringSet(KEY_MUTED, emptySet()).orEmpty().toMutableSet()
        if (mute) now.add(pkg) else now.remove(pkg)
        prefs(context).edit().putStringSet(KEY_MUTED, now).apply()
        muted = now
    }

    /** Запомнить приложение с подписью, чтобы показать его в настройках. */
    fun remember(context: Context, pkg: String, label: String) {
        val p = prefs(context)
        val entry = "$pkg\u0000$label"
        val seen = p.getStringSet(KEY_SEEN, emptySet()).orEmpty()
        if (entry in seen) return
        val updated = seen.filterNot { it.substringBefore('\u0000') == pkg }.toMutableSet().apply { add(entry) }
        p.edit().putStringSet(KEY_SEEN, updated).apply()
    }

    /** (пакет, название), по алфавиту. */
    fun seen(context: Context): List<Pair<String, String>> =
        prefs(context).getStringSet(KEY_SEEN, emptySet()).orEmpty()
            .map { it.substringBefore('\u0000') to it.substringAfter('\u0000') }
            .sortedBy { it.second.lowercase() }
}
