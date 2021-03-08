//! Helper functions for parsing and dealing with ICS calendars.

use anyhow::{anyhow, bail, Context, Error};
use chrono::{Duration, Utc};
use ics_parser::{components::VCalendar, parser};
use reqwest::Method;
use tracing::{error, info, instrument, Span};

use std::{borrow::Cow, convert::TryInto, ops::Deref, str::FromStr};

use crate::database::{Attendee, Event, EventInstance};

/// Parse a ICS encoded calendar.
fn decode_calendar(cal_body: &str) -> Result<Vec<VCalendar>, Error> {
    let components =
        parser::Component::from_str_to_stream(&cal_body).with_context(|| "decoding component")?;

    components
        .into_iter()
        .map(|comp| comp.try_into().with_context(|| "decoding VCALENDAR"))
        .collect()
}

/// Fetch a calendar from a CalDAV URL and parse the returned set of calendars.
///
/// Note that CalDAV returns a calendar per event, rather than one calendar with
/// many events.
#[instrument(skip(client, password), fields(status))]
pub async fn fetch_calendars(
    client: &reqwest::Client,
    url: &str,
    user_name: Option<&str>,
    password: Option<&str>,
) -> Result<Vec<VCalendar>, Error> {
    let mut req = client
        .request(Method::from_str("REPORT").expect("method"), url)
        .header("Content-Type", "application/xml");

    if let Some(user) = user_name {
        req = req.basic_auth(user, password);
    }

    let resp = req
        .body(format!(
            r#"
        <c:calendar-query xmlns:d="DAV:" xmlns:c="urn:ietf:params:xml:ns:caldav">
            <d:prop>
                <d:getetag />
                <c:calendar-data />
            </d:prop>
            <c:filter>
                <c:comp-filter name="VCALENDAR">
                    <c:comp-filter name="VEVENT" >
                    <c:time-range start="{start}" />
                    </c:comp-filter>
                </c:comp-filter>
            </c:filter>
        </c:calendar-query>
        "#,
            start = Utc::now().format("%Y%m%dT%H%M%SZ")
        ))
        .send()
        .await?;

    let status = resp.status();

    let body = resp.text().await?;

    info!(status = status.as_u16(), "Got result from CalDAV");
    Span::current().record("status", &status.as_u16());

    if !status.is_success() {
        bail!("Got {} result from CalDAV", status.as_u16());
    }

    let doc = roxmltree::Document::parse(&body)
        .map_err(|e| anyhow!(e))
        .with_context(|| "decoding xml")?;

    let mut calendars = Vec::new();

    for node in doc.descendants() {
        if node.tag_name().name() != "calendar-data" {
            continue;
        }

        let cal_body = if let Some(t) = node.text() {
            t
        } else {
            continue;
        };

        match decode_calendar(cal_body) {
            Ok(cals) => calendars.extend(cals),
            Err(e) => error!(
                error = e.deref() as &dyn std::error::Error,
                "Failed to parse event"
            ),
        }
    }

    Ok(calendars)
}

/// Parse the calendars into events and event instances.
pub fn parse_calendars_to_events(
    calendar_id: i64,
    calendars: &[VCalendar],
) -> Result<(Vec<Event<'_>>, Vec<EventInstance<'_>>), Error> {
    let now = Utc::now();
    let mut events = Vec::new();
    let mut next_dates = Vec::new();
    for calendar in calendars {
        for (uid, event) in &calendar.events {
            if event.base_event.is_full_day_event() || event.base_event.is_floating_event() {
                continue;
            }

            events.push(Event {
                calendar_id,
                event_id: uid.into(),
                summary: event.base_event.summary.as_deref().map(Cow::from),
                description: event.base_event.description.as_deref().map(Cow::from),
                location: event.base_event.location.as_deref().map(Cow::from),
            });

            // Loop through all occurrences of the event in the next N days and
            // generate `EventInstance` for them.
            for (date, recur_event) in event
                .recur_iter(&calendar)?
                .skip_while(|(d, _)| *d < now)
                .take_while(|(d, _)| *d < now + Duration::days(30))
            {
                let mut attendees = Vec::new();

                // Loop over all the properties to pull out the attendee info.
                'prop_loop: for prop in &recur_event.properties {
                    if let ics_parser::property::Property::Attendee(prop) = prop {
                        if prop.value.scheme() != "mailto" {
                            continue;
                        }

                        let email = prop.value.path().to_string();

                        let mut common_name = None;
                        for param in prop.parameters.parameters() {
                            match param {
                                ics_parser::parameters::Parameter::CN(cn) => {
                                    common_name = Some(cn.clone());
                                }
                                ics_parser::parameters::Parameter::ParticipationStatus(status)
                                    if status == "DECLINED" =>
                                {
                                    continue 'prop_loop;
                                }
                                _ => {}
                            }
                        }

                        attendees.push(Attendee { email, common_name })
                    }
                }

                next_dates.push(EventInstance {
                    event_id: uid.into(),
                    date,
                    attendees,
                });
            }
        }
    }
    Ok((events, next_dates))
}
