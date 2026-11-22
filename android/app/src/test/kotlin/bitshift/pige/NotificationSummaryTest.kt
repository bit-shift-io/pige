package bitshift.pige

import org.junit.Assert.assertEquals
import org.junit.Assert.assertNotEquals
import org.junit.Test

class NotificationSummaryTest {
    private fun entry(sender: String, subject: String, postTime: Long) =
        NotificationLine(sender, subject, postTime)

    @Test
    fun `title singular and plural`() {
        assertEquals("1 new message", NotificationSummary.title(1))
        assertEquals("3 new messages", NotificationSummary.title(3))
    }

    @Test
    fun `lines are newest first`() {
        val lines = NotificationSummary.lines(
            listOf(
                entry("Older", "One", 10),
                entry("Newest", "Two", 30),
                entry("Middle", "Three", 20),
            )
        )
        assertEquals(
            listOf("Newest — Two", "Middle — Three", "Older — One"),
            lines,
        )
    }

    @Test
    fun `lines cap at five and summarize the rest`() {
        val entries = (1..7).map { entry("Sender $it", "Subject $it", it.toLong()) }
        val lines = NotificationSummary.lines(entries)
        assertEquals(5, lines.size)
        assertEquals("Sender 7 — Subject 7", lines.first())
        assertEquals("and 3 more", lines.last())
    }

    @Test
    fun `blank sender and subject use fallbacks`() {
        assertEquals(
            listOf("New mail — (no subject)"),
            NotificationSummary.lines(listOf(entry("", "", 1))),
        )
    }

    @Test
    fun `summary id is namespaced away from child ids`() {
        assertNotEquals(
            NotificationSummary.summaryId("account-1"),
            NotificationSummary.childId("account-1", "thread-1"),
        )
    }
}
