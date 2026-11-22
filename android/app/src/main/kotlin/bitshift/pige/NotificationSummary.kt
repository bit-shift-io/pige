package bitshift.pige

/** One live child notification, reduced to what the group summary shows. */
data class NotificationLine(
    val sender: String,
    val subject: String,
    val postTime: Long,
)

/// Pure layout and id rules for grouped mail notifications. Kept free of
/// Android classes so the JVM unit tests can pin the summary text.
object NotificationSummary {
    const val MAX_LINES = 5

    fun title(count: Int): String =
        if (count == 1) "1 new message" else "$count new messages"

    fun lines(entries: List<NotificationLine>): List<String> {
        val newestFirst = entries.sortedByDescending { it.postTime }
        val shown = if (newestFirst.size <= MAX_LINES) {
            newestFirst
        } else {
            newestFirst.take(MAX_LINES - 1)
        }
        val lines = shown.map { line(it) }.toMutableList()
        if (newestFirst.size > MAX_LINES) {
            lines += "and ${newestFirst.size - shown.size} more"
        }
        return lines
    }

    fun groupKey(account: String): String = "account:$account"

    fun childId(account: String, thread: String): Int =
        "$account\u0000$thread".hashCode()

    fun summaryId(account: String): Int =
        "summary\u0000$account".hashCode()

    private fun line(entry: NotificationLine): String {
        val sender = entry.sender.ifBlank { "New mail" }
        val subject = entry.subject.ifBlank { "(no subject)" }
        return "$sender — $subject"
    }
}
