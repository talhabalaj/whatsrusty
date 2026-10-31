//! Store side of polls: each poll's secret and everyone's current vote.

use anyhow::Result;
use rusqlite::{OptionalExtension as _, params};

use super::Store;
use crate::types::PollVote;

/// Option names are stored joined by the ASCII unit separator (never in option text).
const SEP: char = '\u{1f}';

fn join(options: &[String]) -> String {
    options.join(&SEP.to_string())
}

fn split(stored: &str) -> Vec<String> {
    if stored.is_empty() { Vec::new() } else { stored.split(SEP).map(String::from).collect() }
}

impl Store {
    /// Remember the secret poll `id`'s votes are encrypted with (from its message).
    pub fn set_poll_secret(&self, chat_jid: &str, id: &str, secret: &[u8]) -> Result<()> {
        let chat = self.canonical(chat_jid)?;
        self.conn.execute(
            "INSERT OR IGNORE INTO poll_secrets (chat_jid, message_id, secret) VALUES (?1, ?2, ?3)",
            params![chat, id, secret],
        )?;
        Ok(())
    }

    pub fn poll_secret(&self, chat_jid: &str, id: &str) -> Result<Option<Vec<u8>>> {
        let chat = self.canonical(chat_jid)?;
        Ok(self
            .conn
            .prepare_cached("SELECT secret FROM poll_secrets WHERE chat_jid = ?1 AND message_id = ?2")?
            .query_row(params![chat, id], |r| r.get(0))
            .optional()?)
    }

    /// `voter` chose `options` (names; none: took their vote back) in poll `id` at
    /// `ts_ms`. Only a newer vote replaces theirs. Returns whether it changed.
    pub fn record_poll_vote(
        &self,
        chat_jid: &str,
        id: &str,
        voter: &str,
        from_me: bool,
        options: &[String],
        ts_ms: i64,
    ) -> Result<bool> {
        let (chat, voter) = (self.canonical(chat_jid)?, self.canonical(voter)?);
        let changed = self.conn.execute(
            "INSERT INTO poll_votes (chat_jid, message_id, voter_jid, from_me, options, ts_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT (chat_jid, message_id, voter_jid) DO UPDATE SET
                options = excluded.options, ts_ms = excluded.ts_ms, from_me = excluded.from_me
             WHERE excluded.ts_ms >= ts_ms",
            params![chat, id, voter, from_me, join(options), ts_ms],
        )?;
        Ok(changed > 0)
    }

    /// My current vote in poll `id`, and when (to put back if sending a new one fails).
    pub fn my_poll_vote(&self, chat_jid: &str, id: &str) -> Result<Option<(String, Vec<String>, i64)>> {
        let chat = self.canonical(chat_jid)?;
        let row: Option<(String, String, i64)> = self
            .conn
            .prepare_cached(
                "SELECT voter_jid, options, ts_ms FROM poll_votes WHERE chat_jid = ?1 AND message_id = ?2 AND from_me = 1
                 ORDER BY ts_ms DESC LIMIT 1",
            )?
            .query_row(params![chat, id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))
            .optional()?;
        Ok(row.map(|(voter, options, ts)| (voter, split(&options), ts)))
    }

    /// Put back my vote as it was before one I sent failed (`None`: I hadn't voted).
    pub fn restore_my_poll_vote(
        &self,
        chat_jid: &str,
        id: &str,
        voter: &str,
        previous: Option<(Vec<String>, i64)>,
    ) -> Result<()> {
        let (chat, voter) = (self.canonical(chat_jid)?, self.canonical(voter)?);
        match previous {
            Some((options, ts)) => {
                self.conn.execute(
                    "UPDATE poll_votes SET options = ?4, ts_ms = ?5 WHERE chat_jid = ?1 AND message_id = ?2 AND voter_jid = ?3",
                    params![chat, id, voter, join(&options), ts],
                )?;
            }
            None => {
                self.conn.execute(
                    "DELETE FROM poll_votes WHERE chat_jid = ?1 AND message_id = ?2 AND voter_jid = ?3",
                    params![chat, id, voter],
                )?;
            }
        }
        Ok(())
    }

    /// Everyone's current vote in poll `id`, as indices into its `options` (by name;
    /// options it no longer has are dropped), oldest vote first.
    pub(super) fn poll_votes(&self, chat: &str, id: &str, options: &[String]) -> Result<Vec<PollVote>> {
        let rows: Vec<(String, bool, String)> = self
            .conn
            .prepare_cached(
                "SELECT voter_jid, from_me, options FROM poll_votes WHERE chat_jid = ?1 AND message_id = ?2 ORDER BY ts_ms",
            )?
            .query_map(params![chat, id], |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)))?
            .collect::<rusqlite::Result<_>>()?;
        let mut votes = Vec::new();
        for (voter, from_me, chosen) in rows {
            let chosen: Vec<String> = split(&chosen);
            let picked: Vec<usize> =
                options.iter().enumerate().filter(|(_, o)| chosen.contains(o)).map(|(i, _)| i).collect();
            if picked.is_empty() {
                continue;
            }
            let name = if from_me { "You".to_string() } else { self.sender_display_name(&voter, None)? };
            votes.push(PollVote { voter_jid: voter, name, from_me, options: picked });
        }
        Ok(votes)
    }
}
