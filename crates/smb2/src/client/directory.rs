//! [`DirectoryReader`]: a directory listed one server batch at a time.
//!
//! A listing is CREATE, QUERY_DIRECTORY until `STATUS_NO_MORE_FILES`, CLOSE.
//! [`Tree::list_directory`] runs it to the end and returns everything; this
//! hands each QUERY_DIRECTORY answer to the caller as it arrives, so a folder
//! of 100,000 entries can be shown, filtered, or abandoned after its first
//! batch, and never has to sit in memory whole. `list_directory` collects
//! through a reader, so the two are the same requests on the wire.
//!
//! **Cancel safety is the part that takes care.** QUERY_DIRECTORY is stateful
//! on the server: the handle's cursor moves on when the server answers, and a
//! later query gets the entries after it. So a dropped `next_batch` whose query
//! already reached the wire must not take that answer with it, or the next
//! call silently skips a batch. Consumers drop futures in `select!` routinely,
//! so every request the reader makes is a boxed future it keeps in its own
//! state, and the next call resumes the same one. The `Connection`'s own drop
//! safety isn't enough for this: it keeps the connection consistent by
//! discarding a dropped caller's answer, which is exactly the loss to avoid.

use std::sync::Arc;
use std::time::{Duration, Instant};

use futures_util::future::BoxFuture;
use log::{debug, trace};

use crate::client::connection::Connection;
use crate::client::tree::{DirectoryEntry, QueryStep, QueryStepOutcome, Tree};
use crate::error::Result;
use crate::types::FileId;
use crate::Error;

/// A directory listed one QUERY_DIRECTORY answer at a time.
///
/// Each [`next_batch`](Self::next_batch) sends one QUERY_DIRECTORY and returns
/// the entries in its answer. How many that is depends on the server and the
/// names (about 64 KiB of entries, often several hundred); it isn't a page
/// size to rely on. Names come back mapped exactly as
/// [`Tree::list_directory`] returns them, `.` and `..` included.
///
/// The end of the listing closes the handle by itself. To stop before then,
/// call [`close`](Self::close): Rust has no async drop, so a reader dropped
/// part-way leaks its handle until the session ends, and logs a debug line.
///
/// [`next_batch`](Self::next_batch) is cancel-safe: drop it in a `select!` and
/// the next call returns the batch the dropped one asked for, never the one
/// after it.
///
/// A reader belongs to the session it was opened on. After a reconnect it
/// can't resume, since the server's cursor went with the old session, so
/// `next_batch` fails with [`Error::HandleInvalidated`] without sending
/// anything, and [`close`](Self::close) succeeds without sending anything.
/// Open a new reader to list the directory again.
///
/// Owns a cheap clone of the [`Connection`] and of the [`Tree`], so it
/// borrows nothing and can move to another task.
///
/// ```no_run
/// # async fn example(client: &mut smb2::SmbClient, share: &mut smb2::Tree) -> Result<(), smb2::Error> {
/// let mut reader = client.open_directory_reader(share, "photos").await?;
/// while let Some(batch) = reader.next_batch().await? {
///     for entry in batch {
///         println!("{}", entry.name);
///     }
/// }
/// # Ok(())
/// # }
/// ```
pub struct DirectoryReader {
    tree: Arc<Tree>,
    /// Pinned to the session that opened the handle, before the CREATE.
    conn: Connection,
    file_id: FileId,
    buffer_len: u32,
    state: State,
    /// What [`Tree::list_directory_instrumented`] reports, when it asked.
    timings: Option<Timings>,
}

/// Where a reader is in its listing. A request in flight lives here, not in
/// the caller's future, which is what makes every await on it cancel-safe.
enum State {
    /// The handle is open and nothing is in flight. `restart` asks the server
    /// to start the scan over, which only the first query does.
    Ready { restart: bool },
    /// A QUERY_DIRECTORY, sent (or about to be, on its first poll).
    Querying {
        query: BoxFuture<'static, Result<QueryStepOutcome>>,
        started: Instant,
    },
    /// The CLOSE, and the error to report once it's done: a failed query
    /// closes the handle too, and its error outranks the CLOSE's.
    Closing {
        close: BoxFuture<'static, Result<()>>,
        started: Instant,
        then: Option<Error>,
    },
    /// The handle is closed.
    Done,
}

/// Round-trip times, for [`Tree::list_directory_instrumented`].
#[derive(Default)]
pub(crate) struct Timings {
    pub(crate) queries: Vec<QueryStep>,
    pub(crate) close: Duration,
}

impl DirectoryReader {
    /// Wrap a directory handle `conn` opened. `conn` must already be pinned to
    /// that session.
    pub(crate) fn new(tree: Arc<Tree>, conn: Connection, file_id: FileId, buffer_len: u32) -> Self {
        Self {
            tree,
            conn,
            file_id,
            buffer_len,
            state: State::Ready { restart: true },
            timings: None,
        }
    }

    /// Time every round trip from here on (see [`take_timings`](Self::take_timings)).
    pub(crate) fn record_timings(&mut self) {
        self.timings = Some(Timings::default());
    }

    /// What [`record_timings`](Self::record_timings) has collected so far.
    pub(crate) fn take_timings(&mut self) -> Timings {
        self.timings.take().unwrap_or_default()
    }

    /// The next batch of entries: one QUERY_DIRECTORY answer.
    ///
    /// `Ok(None)` at the end of the listing, once the handle is closed, and on
    /// every call after it. A batch is never empty.
    ///
    /// An error closes the handle too, and the calls after it return
    /// `Ok(None)`; listing the directory again needs a new reader. After a
    /// reconnect the error is [`Error::HandleInvalidated`] (see the type-level
    /// docs).
    ///
    /// Cancel-safe: a call dropped while its request is out leaves the request
    /// with the reader, and the next call picks up its answer.
    pub async fn next_batch(&mut self) -> Result<Option<Vec<DirectoryEntry>>> {
        loop {
            match &mut self.state {
                State::Done => return Ok(None),
                State::Ready { restart } => {
                    let restart = *restart;
                    self.start_query(restart);
                }
                State::Querying { query, started } => {
                    let outcome = query.as_mut().await;
                    let elapsed = started.elapsed();
                    // Nothing below awaits until the state has moved on, so a
                    // drop can't land between taking the answer and recording
                    // that it was taken.
                    match outcome {
                        Ok(QueryStepOutcome::Entries { entries, bytes }) => {
                            self.note_query(elapsed, entries.len(), bytes, false);
                            self.state = State::Ready { restart: false };
                            trace!("directory: batch of {} entries", entries.len());
                            // A server may answer with a buffer that holds no
                            // whole entry; that's no batch to hand out.
                            if !entries.is_empty() {
                                return Ok(Some(entries));
                            }
                        }
                        Ok(QueryStepOutcome::NoMoreFiles { bytes }) => {
                            self.note_query(elapsed, 0, bytes, true);
                            self.start_close(None);
                        }
                        Err(e) => self.start_close(Some(e)),
                    }
                }
                State::Closing { .. } => {
                    let (closed, then) = self.finish_close().await;
                    return match then {
                        Some(e) => Err(e),
                        None => closed.map(|()| None),
                    };
                }
            }
        }
    }

    /// Close the handle before the end of the listing.
    ///
    /// A query a dropped [`next_batch`](Self::next_batch) left out is answered
    /// first, so the QUERY_DIRECTORY and the CLOSE can't race on the handle.
    /// Nothing to do once the listing ended, and nothing sent after a
    /// reconnect (the server let go of the handle with the session); both
    /// return `Ok`.
    pub async fn close(mut self) -> Result<()> {
        if let State::Querying { query, .. } = &mut self.state {
            // Whatever it says, the handle is still open.
            let _ = query.as_mut().await;
            self.state = State::Ready { restart: false };
        }
        match self.state {
            State::Done => return Ok(()),
            State::Ready { .. } => self.start_close(None),
            State::Querying { .. } | State::Closing { .. } => {}
        }
        self.finish_close().await.0
    }

    fn start_query(&mut self, restart: bool) {
        let tree = Arc::clone(&self.tree);
        let mut conn = self.conn.clone();
        let (file_id, buffer_len) = (self.file_id, self.buffer_len);
        self.state = State::Querying {
            query: Box::pin(async move {
                tree.query_directory_step(&mut conn, file_id, restart, buffer_len)
                    .await
            }),
            started: Instant::now(),
        };
    }

    fn start_close(&mut self, then: Option<Error>) {
        let tree = Arc::clone(&self.tree);
        let mut conn = self.conn.clone();
        let file_id = self.file_id;
        self.state = State::Closing {
            close: Box::pin(async move {
                match tree.close_handle(&mut conn, file_id).await {
                    // The server let go of the handle with its session.
                    Err(Error::HandleInvalidated { .. }) => Ok(()),
                    result => result,
                }
            }),
            started: Instant::now(),
            then,
        };
    }

    /// Await the CLOSE in place, then hand back its result and the error it
    /// was carrying. Cancel-safe like the rest.
    async fn finish_close(&mut self) -> (Result<()>, Option<Error>) {
        let State::Closing { close, started, .. } = &mut self.state else {
            return (Ok(()), None);
        };
        let closed = close.as_mut().await;
        let elapsed = started.elapsed();
        let State::Closing { then, .. } = std::mem::replace(&mut self.state, State::Done) else {
            unreachable!("still closing: nothing awaited since the match above");
        };
        if let Some(timings) = &mut self.timings {
            timings.close = elapsed;
        }
        (closed, then)
    }

    fn note_query(&mut self, elapsed: Duration, entries: usize, bytes: usize, no_more_files: bool) {
        if let Some(timings) = &mut self.timings {
            timings.queries.push(QueryStep {
                elapsed,
                entries,
                bytes,
                no_more_files,
            });
        }
    }
}

impl Drop for DirectoryReader {
    fn drop(&mut self) {
        // A handle whose session ended went with it, so nothing leaks.
        if !matches!(self.state, State::Done) && self.conn.pinned_session_is_current() {
            debug!(
                "directory: DirectoryReader dropped before the end of its listing without \
                 close(), directory handle may leak until session teardown"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use crate::client::test_helpers::{
        build_close_response, build_create_response, build_file_both_dir_info,
        build_query_directory_response, setup_connection,
    };
    use crate::client::tree::Tree;
    use crate::msg::header::Header;
    use crate::msg::query_directory::{QueryDirectoryFlags, QueryDirectoryRequest};
    use crate::pack::{ReadCursor, Unpack};
    use crate::transport::MockTransport;
    use crate::types::status::NtStatus;
    use crate::types::{Command, FileId, TreeId};
    use crate::Error;

    fn test_tree() -> Tree {
        Tree {
            pin: None,
            tree_id: TreeId(10),
            share_name: "test".to_string(),
            server: "test-server".to_string(),
            is_dfs: false,
            encrypt_data: false,
            dfs_origin: None,
        }
    }

    const DIR: FileId = FileId {
        persistent: 0x1111,
        volatile: 0x2222,
    };

    /// One QUERY_DIRECTORY answer carrying a file per name, as the server
    /// writes them: chained by `NextEntryOffset`, names in wire form.
    fn batch(names: &[&str]) -> Vec<u8> {
        let mut data = Vec::new();
        for (i, name) in names.iter().enumerate() {
            let wire = crate::name::encode_name(name);
            let last = i + 1 == names.len();
            let len = build_file_both_dir_info(&wire, 1, false, 0).len();
            // Entries are 8-byte aligned in a real answer.
            let next = if last {
                0
            } else {
                len.next_multiple_of(8) as u32
            };
            let mut entry = build_file_both_dir_info(&wire, 1, false, next);
            if !last {
                entry.resize(next as usize, 0);
            }
            data.extend_from_slice(&entry);
        }
        build_query_directory_response(NtStatus::SUCCESS, data)
    }

    fn no_more_files() -> Vec<u8> {
        build_query_directory_response(NtStatus::NO_MORE_FILES, vec![])
    }

    fn names(entries: &[crate::DirectoryEntry]) -> Vec<&str> {
        entries.iter().map(|e| e.name.as_str()).collect()
    }

    /// Every command the mock saw, in send order.
    fn sent_commands(mock: &MockTransport) -> Vec<Command> {
        mock.sent_messages()
            .iter()
            .map(|bytes| Header::unpack(&mut ReadCursor::new(bytes)).unwrap().command)
            .collect()
    }

    /// Whether each QUERY_DIRECTORY the mock saw asked to restart the scan.
    fn query_restarts(mock: &MockTransport) -> Vec<bool> {
        mock.sent_messages()
            .iter()
            .filter(|bytes| {
                Header::unpack(&mut ReadCursor::new(bytes)).unwrap().command
                    == Command::QueryDirectory
            })
            .map(|bytes| {
                let req =
                    QueryDirectoryRequest::unpack(&mut ReadCursor::new(&bytes[Header::SIZE..]))
                        .unwrap();
                req.flags.0 & QueryDirectoryFlags::RESTART_SCANS != 0
            })
            .collect()
    }

    /// Compiles only while a reader and its calls can move to another task,
    /// which a consumer spawning a listing needs.
    #[allow(dead_code)]
    fn a_reader_and_its_calls_stay_send(mut reader: super::DirectoryReader) {
        fn send<T: Send>(_: &T) {}
        send(&reader.next_batch());
        send(&reader);
        send(&reader.close());
    }

    #[tokio::test]
    async fn each_batch_is_one_query_directory_answer_and_the_end_closes_the_handle() {
        let mock = Arc::new(MockTransport::new());
        mock.queue_response(build_create_response(DIR, 0));
        mock.queue_response(batch(&[".", "..", "a.txt"]));
        // A name SMB2 can't carry comes back mapped (AGENTS.md pitfall 23).
        mock.queue_response(batch(&["what?.txt"]));
        mock.queue_response(no_more_files());
        mock.queue_response(build_close_response());

        let conn = setup_connection(&mock);
        let mut reader = test_tree()
            .open_directory_reader(conn, "somedir")
            .await
            .unwrap();

        let first = reader.next_batch().await.unwrap().unwrap();
        assert_eq!(names(&first), [".", "..", "a.txt"]);
        assert_eq!(
            sent_commands(&mock),
            [Command::Create, Command::QueryDirectory],
            "a batch costs one QUERY_DIRECTORY, and nothing goes out ahead of the caller"
        );

        let second = reader.next_batch().await.unwrap().unwrap();
        assert_eq!(names(&second), ["what?.txt"]);

        assert!(reader.next_batch().await.unwrap().is_none());
        assert_eq!(
            sent_commands(&mock),
            [
                Command::Create,
                Command::QueryDirectory,
                Command::QueryDirectory,
                Command::QueryDirectory,
                Command::Close,
            ],
            "STATUS_NO_MORE_FILES closes the handle by itself"
        );
        assert_eq!(query_restarts(&mock), [true, false, false]);

        // Finished stays finished, and costs nothing.
        assert!(reader.next_batch().await.unwrap().is_none());
        reader.close().await.unwrap();
        assert_eq!(mock.sent_count(), 5);
        mock.assert_fully_consumed();
    }

    #[tokio::test]
    async fn closing_early_sends_a_close_and_no_more_queries() {
        let mock = Arc::new(MockTransport::new());
        mock.queue_response(build_create_response(DIR, 0));
        mock.queue_response(batch(&["a", "b"]));
        mock.queue_response(build_close_response());

        let conn = setup_connection(&mock);
        let mut reader = test_tree()
            .open_directory_reader(conn, "somedir")
            .await
            .unwrap();
        assert_eq!(
            names(&reader.next_batch().await.unwrap().unwrap()),
            ["a", "b"]
        );
        reader.close().await.unwrap();

        assert_eq!(
            sent_commands(&mock),
            [Command::Create, Command::QueryDirectory, Command::Close]
        );
        mock.assert_fully_consumed();
    }

    #[tokio::test]
    async fn a_refused_query_closes_the_handle_and_reports_the_refusal() {
        let mock = Arc::new(MockTransport::new());
        mock.queue_response(build_create_response(DIR, 0));
        mock.queue_response(build_query_directory_response(
            NtStatus::ACCESS_DENIED,
            vec![],
        ));
        mock.queue_response(build_close_response());

        let conn = setup_connection(&mock);
        let mut reader = test_tree()
            .open_directory_reader(conn, "somedir")
            .await
            .unwrap();
        assert!(matches!(
            reader.next_batch().await,
            Err(Error::Protocol {
                status: NtStatus::ACCESS_DENIED,
                command: Command::QueryDirectory,
            })
        ));
        assert_eq!(
            sent_commands(&mock),
            [Command::Create, Command::QueryDirectory, Command::Close],
            "the handle doesn't leak behind an error"
        );
        assert!(reader.next_batch().await.unwrap().is_none());
        assert_eq!(mock.sent_count(), 3);
    }

    #[tokio::test]
    async fn dropping_next_batch_mid_query_keeps_that_batch() {
        // QUERY_DIRECTORY is stateful: the server's cursor moved when it
        // answered. A reader that sent a fresh query after a `select!` arm lost
        // would get the batch AFTER the one it dropped, silently.
        let mock = Arc::new(MockTransport::new());
        mock.queue_response(build_create_response(DIR, 0));
        let conn = setup_connection(&mock);
        let mut reader = test_tree()
            .open_directory_reader(conn, "somedir")
            .await
            .unwrap();

        let dropped = tokio::time::timeout(Duration::from_millis(50), reader.next_batch()).await;
        assert!(dropped.is_err(), "nothing was answered yet");
        assert_eq!(
            sent_commands(&mock),
            [Command::Create, Command::QueryDirectory]
        );

        mock.queue_response(batch(&["first"]));
        mock.queue_response(batch(&["second"]));
        mock.queue_response(no_more_files());
        mock.queue_response(build_close_response());

        let resumed = reader.next_batch().await.unwrap().unwrap();
        assert_eq!(
            names(&resumed),
            ["first"],
            "the batch the dropped call asked for"
        );
        assert_eq!(
            names(&reader.next_batch().await.unwrap().unwrap()),
            ["second"]
        );
        assert!(reader.next_batch().await.unwrap().is_none());
        assert_eq!(query_restarts(&mock), [true, false, false]);
        mock.assert_fully_consumed();
    }

    #[tokio::test]
    async fn dropping_next_batch_mid_close_sends_no_second_close() {
        let mock = Arc::new(MockTransport::new());
        mock.queue_response(build_create_response(DIR, 0));
        mock.queue_response(no_more_files());
        let conn = setup_connection(&mock);
        let mut reader = test_tree()
            .open_directory_reader(conn, "somedir")
            .await
            .unwrap();

        let dropped = tokio::time::timeout(Duration::from_millis(50), reader.next_batch()).await;
        assert!(dropped.is_err(), "the CLOSE wasn't answered yet");
        assert_eq!(
            sent_commands(&mock),
            [Command::Create, Command::QueryDirectory, Command::Close]
        );

        mock.queue_response(build_close_response());
        assert!(reader.next_batch().await.unwrap().is_none());
        assert_eq!(
            mock.sent_count(),
            3,
            "the CLOSE already out is the one awaited"
        );
        mock.assert_fully_consumed();
    }

    #[tokio::test]
    async fn closing_after_a_dropped_next_batch_waits_for_its_query_first() {
        // Two requests on one stateful handle must not race: the CLOSE goes out
        // only once the query the dropped call left behind is answered.
        let mock = Arc::new(MockTransport::new());
        mock.queue_response(build_create_response(DIR, 0));
        let conn = setup_connection(&mock);
        let mut reader = test_tree()
            .open_directory_reader(conn, "somedir")
            .await
            .unwrap();

        let dropped = tokio::time::timeout(Duration::from_millis(50), reader.next_batch()).await;
        assert!(dropped.is_err());
        mock.queue_response(batch(&["unread"]));
        mock.queue_response(build_close_response());
        reader.close().await.unwrap();

        assert_eq!(
            sent_commands(&mock),
            [Command::Create, Command::QueryDirectory, Command::Close]
        );
        mock.assert_fully_consumed();
    }
}
