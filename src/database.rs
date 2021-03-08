//! Module for talking to the database

use std::{
    borrow::Cow,
    collections::{BTreeMap, VecDeque},
};

use anyhow::Error;
use chrono::{DateTime, Duration, FixedOffset, Utc};
use postgres_types::{FromSql, ToSql};
use serde::{Deserialize, Serialize};
use tokio_postgres::NoTls;

/// Async database pool for PostgreSQL.
pub type PostgresPool = bb8::Pool<bb8_postgres::PostgresConnectionManager<NoTls>>;

/// An attendee of the meeting.
///
/// Includes people who haven't responded, or are tentative/confirmed.
#[derive(Debug, Clone, ToSql, FromSql)]
pub struct Attendee {
    pub email: String,
    pub common_name: Option<String>,
}

/// The URL and credentials of a calendar.
#[derive(Debug, Clone, Serialize)]
pub struct Calendar {
    pub calendar_id: i64,
    pub name: String,
    pub url: String,
    pub user_name: Option<String>,
    pub password: Option<String>,
}

/// Basic info for an event.
#[derive(Debug, Clone)]
pub struct Event<'a> {
    pub calendar_id: i64,
    pub event_id: Cow<'a, str>,
    pub summary: Option<Cow<'a, str>>,
    pub description: Option<Cow<'a, str>>,
    pub location: Option<Cow<'a, str>>,
}

/// A particular instance of an event, with date/time and attendees.
#[derive(Debug, Clone)]
pub struct EventInstance<'a> {
    pub event_id: Cow<'a, str>,
    pub date: DateTime<FixedOffset>,
    pub attendees: Vec<Attendee>,
}

/// A reminder for a particular [`EventInstance`]
#[derive(Debug, Clone)]
pub struct ReminderInstance {
    pub event_id: String,
    pub summary: Option<String>,
    pub description: Option<String>,
    pub location: Option<String>,
    pub template: Option<String>,
    pub minutes_before: i64,
    pub room_id: String,
    pub attendees: Vec<Attendee>,
}

/// A configured reminder
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Reminder {
    pub reminder_id: i64,
    pub calendar_id: i64,
    pub event_id: String,
    pub template: Option<String>,
    pub minutes_before: i64,
    pub room_id: String,
}

/// Allows talking to the database.
#[derive(Debug, Clone)]
pub struct Database {
    db_pool: PostgresPool,
}

impl Database {
    /// Create a new `Database` from a PostgreSQL connection pool.
    pub fn from_pool(db_pool: PostgresPool) -> Database {
        Database { db_pool }
    }

    /// Fetch stored calendar info.
    pub async fn get_calendars(&self) -> Result<Vec<Calendar>, Error> {
        let db_conn = self.db_pool.get().await?;

        let rows = db_conn
            .query(
                "SELECT calendar_id, name, url, user_name, password FROM calendars",
                &[],
            )
            .await?;

        let mut calendars = Vec::with_capacity(rows.len());
        for row in rows {
            let calendar_id = row.try_get("calendar_id")?;
            let name = row.try_get("name")?;
            let url = row.try_get("url")?;
            let user_name = row.try_get("user_name")?;
            let password = row.try_get("password")?;

            calendars.push(Calendar {
                calendar_id,
                name,
                url,
                user_name,
                password,
            })
        }

        Ok(calendars)
    }

    pub async fn get_calendars_for_user(&self, user_id: i64) -> Result<Vec<Calendar>, Error> {
        let db_conn = self.db_pool.get().await?;

        let rows = db_conn
            .query(
                r#"
                    SELECT calendar_id, name, url, user_name, password FROM calendars
                    WHERE user_id = $1
                "#,
                &[&user_id],
            )
            .await?;

        let mut calendars = Vec::with_capacity(rows.len());
        for row in rows {
            let calendar_id = row.try_get("calendar_id")?;
            let name = row.try_get("name")?;
            let url = row.try_get("url")?;
            let user_name = row.try_get("user_name")?;
            let password = row.try_get("password")?;

            calendars.push(Calendar {
                calendar_id,
                name,
                url,
                user_name,
                password,
            })
        }

        Ok(calendars)
    }

    pub async fn get_calendar(&self, calendar_id: i64) -> Result<Option<Calendar>, Error> {
        let db_conn = self.db_pool.get().await?;

        let row = db_conn
            .query_opt(
                r#"
                    SELECT calendar_id, name, url, user_name, password FROM calendars
                    WHERE calendar_id = $1
                "#,
                &[&calendar_id],
            )
            .await?;

        if let Some(row) = row {
            let calendar_id = row.try_get("calendar_id")?;
            let name = row.try_get("name")?;
            let url = row.try_get("url")?;
            let user_name = row.try_get("user_name")?;
            let password = row.try_get("password")?;

            Ok(Some(Calendar {
                calendar_id,
                name,
                url,
                user_name,
                password,
            }))
        } else {
            Ok(None)
        }
    }

    pub async fn update_calendar(
        &self,
        calendar_id: i64,
        name: String,
        url: String,
        user_name: Option<String>,
        password: Option<String>,
    ) -> Result<(), Error> {
        let db_conn = self.db_pool.get().await?;

        db_conn
            .execute(
                r#"
                    UPDATE calendars
                    SET name = $2, url = $3, user_name = $4, password = $5
                    WHERE calendar_id = $1
                "#,
                &[&calendar_id, &name, &url, &user_name, &password],
            )
            .await?;

        Ok(())
    }

    pub async fn add_calendar(
        &self,
        user_id: i64,
        name: String,
        url: String,
        user_name: Option<String>,
        password: Option<String>,
    ) -> Result<i64, Error> {
        let db_conn = self.db_pool.get().await?;

        let row = db_conn
            .query_one(
                r#"
                    INSERT INTO calendars (user_id, name, url, user_name, password)
                    VALUES ($1, $2, $3, $4, $5)
                    RETURNING calendar_id
                "#,
                &[&user_id, &name, &url, &user_name, &password],
            )
            .await?;

        Ok(row.try_get(0)?)
    }

    /// Insert events and the next instances of the event.
    ///
    /// Not all event instances are stored (since they might be infinite),
    /// instead only the instances in the next, say, month are typically stored.
    pub async fn insert_events(
        &self,
        calendar_id: i64,
        events: Vec<Event<'_>>,
        instances: Vec<EventInstance<'_>>,
    ) -> Result<(), Error> {
        let mut db_conn = self.db_pool.get().await?;
        let txn = db_conn.transaction().await?;

        futures::future::try_join_all(events.iter().map(|event| {
            txn.execute_raw(
                r#"
                INSERT INTO events (calendar_id, event_id, summary, description, location)
                VALUES ($1, $2, $3, $4, $5)
                ON CONFLICT (calendar_id, event_id)
                DO UPDATE SET
                    summary = EXCLUDED.summary,
                    description = EXCLUDED.description,
                    location = EXCLUDED.location
            "#,
                vec![
                    &calendar_id as &dyn ToSql,
                    &event.event_id,
                    &event.summary,
                    &event.description,
                    &event.location,
                ],
            )
        }))
        .await?;

        txn.execute(
            "DELETE FROM next_dates WHERE calendar_id = $1",
            &[&calendar_id],
        )
        .await?;

        futures::future::try_join_all(instances.iter().map(|instance| {
            txn.execute_raw(
                r#"
                            INSERT INTO next_dates (calendar_id, event_id, timestamp, attendees)
                            VALUES ($1, $2, $3, $4)
                        "#,
                vec![
                    &calendar_id as &dyn ToSql,
                    &instance.event_id,
                    &instance.date,
                    &instance.attendees,
                ],
            )
        }))
        .await?;

        txn.commit().await?;

        Ok(())
    }

    pub async fn add_reminder(
        &self,
        user_id: i64,
        calendar_id: i64,
        event_id: &'_ str,
        room_id: &'_ str,
        minutes_before: i64,
        template: Option<&'_ str>,
    ) -> Result<(), Error> {
        let db_conn = self.db_pool.get().await?;

        db_conn
            .execute(
                r#"
                    INSERT INTO reminders (user_id, calendar_id, event_id, room_id, minutes_before, template)
                    VALUES ($1, $2, $3, $4, $5, $6)
            "#,
                &[&user_id, &calendar_id, &event_id, &room_id, &minutes_before, &template],
            )
            .await?;

        Ok(())
    }

    pub async fn update_reminder(
        &self,
        reminder_id: i64,
        room_id: &'_ str,
        minutes_before: i64,
        template: Option<&'_ str>,
    ) -> Result<(), Error> {
        let db_conn = self.db_pool.get().await?;

        db_conn
            .execute(
                r#"
                    UPDATE reminders
                    SET room_id = $1, minutes_before = $2, template = $3
                    WHERE reminder_id = $4
            "#,
                &[&room_id, &minutes_before, &template, &reminder_id],
            )
            .await?;

        Ok(())
    }

    pub async fn delete_reminder(&self, reminder_id: i64) -> Result<(), Error> {
        let db_conn = self.db_pool.get().await?;

        db_conn
            .execute(
                r#"
                    DELETE FROM reminders
                    WHERE reminder_id = $1
            "#,
                &[&reminder_id],
            )
            .await?;

        Ok(())
    }

    /// Get the reminders needed to be sent out.
    pub async fn get_next_reminders(
        &self,
    ) -> Result<VecDeque<(DateTime<Utc>, ReminderInstance)>, Error> {
        let db_conn = self.db_pool.get().await?;

        let rows = db_conn
            .query(
                r#"
                    SELECT event_id, summary, description, location, timestamp, room_id, minutes_before, template, attendees
                    FROM reminders
                    INNER JOIN events USING (calendar_id, event_id)
                    INNER JOIN next_dates USING (calendar_id, event_id)
                    ORDER BY timestamp
                "#,
                &[],
            )
            .await?;

        let mut reminders = VecDeque::with_capacity(rows.len());

        for row in rows {
            let event_id: String = row.get(0);
            let summary: Option<String> = row.get(1);
            let description: Option<String> = row.get(2);
            let location: Option<String> = row.get(3);
            let timestamp: DateTime<Utc> = row.get(4);
            let room_id: String = row.get(5);
            let minutes_before: i64 = row.get(6);
            let template: Option<String> = row.get(7);
            let attendees: Vec<Attendee> = row.get(8);

            let reminder_time = timestamp - Duration::minutes(minutes_before);
            if reminder_time < Utc::now() {
                // XXX: There's technically a race here if we reload the
                // reminders just as we're about to send out a reminder.
                continue;
            }

            let reminder = ReminderInstance {
                event_id,
                summary,
                description,
                location,
                template,
                minutes_before,
                room_id,
                attendees,
            };

            reminders.push_back((reminder_time, reminder));
        }

        reminders.make_contiguous().sort_by_key(|(t, _)| *t);

        Ok(reminders)
    }

    /// Get all events in a calendar
    pub async fn get_events_in_calendar(
        &self,
        calendar_id: i64,
    ) -> Result<Vec<(Event<'static>, Vec<EventInstance<'static>>)>, Error> {
        let db_conn = self.db_pool.get().await?;

        let rows = db_conn
            .query(
                r#"
                    SELECT DISTINCT ON (event_id) event_id, summary, description, location, timestamp, attendees
                    FROM events
                    INNER JOIN next_dates USING (calendar_id, event_id)
                    WHERE calendar_id = $1
                    ORDER BY event_id, timestamp
                "#,
                &[&calendar_id],
            )
            .await?;

        let mut events: Vec<(Event<'static>, Vec<EventInstance<'static>>)> =
            Vec::with_capacity(rows.len());

        for row in rows {
            let event_id: String = row.try_get("event_id")?;
            let summary: Option<String> = row.try_get("summary")?;
            let description: Option<String> = row.try_get("description")?;
            let location: Option<String> = row.try_get("location")?;
            let date: DateTime<FixedOffset> = row.try_get("timestamp")?;
            let attendees: Vec<Attendee> = row.try_get("attendees")?;

            if date < Utc::now() {
                // ignore events in the past
                continue;
            }

            let instance = EventInstance {
                event_id: event_id.clone().into(),
                date,
                attendees,
            };

            if let Some((event, instances)) = events.last_mut() {
                if event.event_id == event_id {
                    instances.push(instance);
                    continue;
                }
            }

            let event = Event {
                calendar_id,
                event_id: event_id.clone().into(),
                summary: summary.map(Cow::from),
                description: description.map(Cow::from),
                location: location.map(Cow::from),
            };
            events.push((event, vec![instance]));
        }

        events.sort_by_key(|(_, i)| i[0].date);

        Ok(events)
    }

    pub async fn get_events_for_user(
        &self,
        user_id: i64,
    ) -> Result<Vec<(Event<'static>, Vec<EventInstance<'static>>)>, Error> {
        let db_conn = self.db_pool.get().await?;

        let rows = db_conn
            .query(
                r#"
                    SELECT DISTINCT ON (calendar_id, event_id) calendar_id, event_id, summary, description, location, timestamp, attendees
                    FROM calendars
                    INNER JOIN events USING (calendar_id)
                    INNER JOIN next_dates USING (calendar_id, event_id)
                    WHERE user_id = $1
                    ORDER BY calendar_id, event_id, timestamp
                "#,
                &[&user_id],
            )
            .await?;

        let mut events: Vec<(Event<'static>, Vec<EventInstance<'static>>)> =
            Vec::with_capacity(rows.len());

        for row in rows {
            let calendar_id: i64 = row.try_get("calendar_id")?;
            let event_id: String = row.try_get("event_id")?;
            let summary: Option<String> = row.try_get("summary")?;
            let description: Option<String> = row.try_get("description")?;
            let location: Option<String> = row.try_get("location")?;
            let date: DateTime<FixedOffset> = row.try_get("timestamp")?;
            let attendees: Vec<Attendee> = row.try_get("attendees")?;

            if date < Utc::now() {
                // ignore events in the past
                continue;
            }

            let instance = EventInstance {
                event_id: event_id.clone().into(),
                date,
                attendees,
            };

            if let Some((event, instances)) = events.last_mut() {
                if event.event_id == event_id {
                    instances.push(instance);
                    continue;
                }
            }

            let event = Event {
                calendar_id,
                event_id: event_id.clone().into(),
                summary: summary.map(Cow::from),
                description: description.map(Cow::from),
                location: location.map(Cow::from),
            };
            events.push((event, vec![instance]));
        }

        events.sort_by_key(|(_, i)| i[0].date);

        Ok(events)
    }

    /// Get the specified event
    pub async fn get_event_in_calendar(
        &self,
        calendar_id: i64,
        event_id: &str,
    ) -> Result<Option<(Event<'static>, Vec<EventInstance<'static>>)>, Error> {
        let db_conn = self.db_pool.get().await?;

        let row = db_conn
            .query_opt(
                r#"
                    SELECT DISTINCT ON (event_id) event_id, summary, description, location
                    FROM events
                    WHERE calendar_id = $1 AND event_id = $2
                "#,
                &[&calendar_id, &event_id],
            )
            .await?;

        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };

        let event_id: String = row.get(0);
        let summary: Option<String> = row.get(1);
        let description: Option<String> = row.get(2);
        let location: Option<String> = row.get(3);

        let event = Event {
            calendar_id,
            event_id: event_id.clone().into(),
            summary: summary.map(Cow::from),
            description: description.map(Cow::from),
            location: location.map(Cow::from),
        };

        let mut instances = Vec::new();

        let rows = db_conn
            .query(
                r#"
                    SELECT timestamp, attendees
                    FROM next_dates
                    WHERE calendar_id = $1 AND event_id = $2
                    ORDER BY timestamp
                "#,
                &[&calendar_id, &event_id],
            )
            .await?;

        for row in rows {
            let date: DateTime<FixedOffset> = row.get(0);
            let attendees: Vec<Attendee> = row.get(1);

            if date < Utc::now() {
                // ignore events in the past
                continue;
            }

            let instance = EventInstance {
                event_id: event_id.clone().into(),
                date,
                attendees,
            };

            instances.push(instance);
        }

        instances.sort_by_key(|i| i.date);

        Ok(Some((event, instances)))
    }

    /// Get reminder for event
    pub async fn get_reminders_for_event(
        &self,
        calendar_id: i64,
        event_id: &str,
    ) -> Result<Vec<Reminder>, Error> {
        let db_conn = self.db_pool.get().await?;

        let rows = db_conn
            .query(
                r#"
                        SELECT reminder_id, room_id, minutes_before, template
                        FROM reminders
                        WHERE calendar_id = $1 AND event_id = $2
                    "#,
                &[&calendar_id, &event_id],
            )
            .await?;

        let mut reminders = Vec::with_capacity(rows.len());
        for row in rows {
            let reminder_id = row.try_get("reminder_id")?;
            let room_id = row.try_get("room_id")?;
            let minutes_before = row.try_get("minutes_before")?;
            let template = row.try_get("template")?;

            let reminder = Reminder {
                reminder_id,
                calendar_id,
                event_id: event_id.to_string(),
                room_id,
                minutes_before,
                template,
            };
            reminders.push(reminder)
        }

        Ok(reminders)
    }

    /// Get a reminder for event
    pub async fn get_reminder(&self, reminder_id: i64) -> Result<Option<Reminder>, Error> {
        let db_conn = self.db_pool.get().await?;

        let row = db_conn
            .query_opt(
                r#"
                        SELECT calendar_id, event_id, reminder_id, room_id, minutes_before, template
                        FROM reminders
                        WHERE reminder_id = $1
                    "#,
                &[&reminder_id],
            )
            .await?;

        let row = if let Some(row) = row {
            row
        } else {
            return Ok(None);
        };

        let calendar_id = row.try_get("calendar_id")?;
        let reminder_id = row.try_get("reminder_id")?;
        let event_id = row.try_get("event_id")?;
        let room_id = row.try_get("room_id")?;
        let minutes_before = row.try_get("minutes_before")?;
        let template = row.try_get("template")?;

        let reminder = Reminder {
            reminder_id,
            calendar_id,
            event_id,
            room_id,
            minutes_before,
            template,
        };

        Ok(Some(reminder))
    }

    /// Get the stored mappings from email to matrix ID.
    pub async fn get_user_mappings(&self) -> Result<BTreeMap<String, String>, Error> {
        let db_conn = self.db_pool.get().await?;

        let rows = db_conn
            .query("SELECT email, matrix_id FROM email_to_matrix_id", &[])
            .await?;

        let mapping: BTreeMap<String, String> = rows
            .into_iter()
            .map(|row| (row.get(0), row.get(1)))
            .collect();

        Ok(mapping)
    }
}
