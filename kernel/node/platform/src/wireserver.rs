//! Azure's "ready" report (system design, section 1F, "`platform` on Azure"; work item 1.12).
//!
//! Once node config is valid on Azure, `platform` tells Azure the machine is provisioned, as
//! Microsoft's "Create Linux images without a provisioning agent" shows it
//! (<https://learn.microsoft.com/en-us/azure/virtual-machines/linux/no-agent>, "Generic steps"):
//!
//! 1. `GET http://168.63.129.16/machine?comp=goalstate`, and from the goal state the text of
//!    `Incarnation`, `Container/ContainerId` and `Container/RoleInstanceList/RoleInstance/
//!    InstanceId`, read as the page's Python reads them (`ElementTree`'s `findtext`: the first
//!    element at each path under the document's root);
//! 2. `POST http://168.63.129.16/machine?comp=health` with the `Health` document that names
//!    them, exactly as that Python writes it.
//!
//! Both carry the header `x-ms-version: 2012-11-30`, as 1F says. The `POST` also carries
//! `Content-Type: text/xml;charset=utf-8` and `x-ms-agent-name: moruna`, as every `POST` Microsoft
//! documents carries a type and an agent's name (PM, item 1.12). Each is answered `200` when it is
//! taken. Without the report, Azure reboots the machine.
//!
//! **A refused report is tried again every 5 s, for up to 2 minutes** (work item 1.12): an
//! attempt begins every 5 s and has until the next one would begin. A refusal is any answer but
//! `200` to either request, a goal state that does not name the three values, or no answer at
//! all. When 2 minutes have passed since the first attempt began, it is fatal.

use alloc::{format, string::String};
use core::net::Ipv4Addr;

use crate::{
    boot::{Fatal, Stage},
    http::{self, HttpError, Request, Response},
    instance::SECOND,
    machine::{Clock, Network},
    xml,
};

/// Azure's WireServer.
pub const ADDRESS: Ipv4Addr = Ipv4Addr::new(168, 63, 129, 16);
/// The port it serves the report on.
pub const PORT: u16 = 80;
const HOST: &str = "168.63.129.16";

/// The header both requests carry (1F).
pub const HEADERS: &[(&str, &str)] = &[("x-ms-version", "2012-11-30")];
/// The headers the `Health` document is posted with: 1F's, the document's type, and this
/// agent's name, in the order Microsoft's Python sends them.
pub const HEALTH_HEADERS: &[(&str, &str)] = &[
    ("x-ms-version", "2012-11-30"),
    ("Content-Type", "text/xml;charset=utf-8"),
    ("x-ms-agent-name", "moruna"),
];
/// The goal state.
pub const GOAL_STATE: Request<'static> = Request::get(HOST, "/machine?comp=goalstate", HEADERS);
/// Where the `Health` document is posted.
pub const HEALTH_PATH: &str = "/machine?comp=health";

/// How long after an attempt began the next one begins, when it was refused.
pub const RETRY_NS: u64 = 5 * SECOND;
/// How long after the first attempt began a refused report is fatal.
pub const LIMIT_NS: u64 = 120 * SECOND;
/// The most bytes of each value the goal state names.
pub const VALUE_MAX: usize = 256;

/// The paths the goal state is read at, under its root element.
const INCARNATION: &[&str] = &["Incarnation"];
const CONTAINER: &[&str] = &["Container", "ContainerId"];
const INSTANCE: &[&str] = &[
    "Container",
    "RoleInstanceList",
    "RoleInstance",
    "InstanceId",
];

/// What the goal state names, for the report.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GoalState {
    /// `Incarnation`.
    pub incarnation: String,
    /// `Container/ContainerId`.
    pub container: String,
    /// `Container/RoleInstanceList/RoleInstance/InstanceId`.
    pub instance: String,
}

impl GoalState {
    /// Reads the goal state's three values: each the text of the first element at its path,
    /// without the blank space around it, and each 1 to [`VALUE_MAX`] printable ASCII
    /// characters with no space. What is wrong otherwise.
    pub fn parse(document: &[u8]) -> Result<GoalState, String> {
        let [incarnation, container, instance] =
            xml::find_texts(document, [INCARNATION, CONTAINER, INSTANCE])
                .map_err(|why| format!("is not XML this node reads: {why}"))?;
        let value = |text: Option<String>, path: &[&str]| -> Result<String, String> {
            let path = path.join("/");
            let text = text.ok_or_else(|| format!("has no {path}"))?;
            let text = text.trim_matches([' ', '\t', '\r', '\n']);
            if text.is_empty() {
                return Err(format!("has an empty {path}"));
            }
            if text.len() > VALUE_MAX {
                return Err(format!("has a {path} over {VALUE_MAX} bytes"));
            }
            if !text.bytes().all(|byte| byte.is_ascii_graphic()) {
                return Err(format!(
                    "has a {path} that is not printable ASCII without spaces"
                ));
            }
            Ok(String::from(text))
        };
        Ok(GoalState {
            incarnation: value(incarnation, INCARNATION)?,
            container: value(container, CONTAINER)?,
            instance: value(instance, INSTANCE)?,
        })
    }

    /// The `Health` document that reports the machine ready, as Microsoft's Python writes it
    /// with `ElementTree.tostring`: no declaration, no blank space, the text escaped.
    pub fn health(&self) -> String {
        format!(
            "<Health><GoalStateIncarnation>{}</GoalStateIncarnation><Container><ContainerId>{}\
             </ContainerId><RoleInstanceList><Role><InstanceId>{}</InstanceId><Health><State>\
             Ready</State></Health></Role></RoleInstanceList></Container></Health>",
            xml::escape(&self.incarnation),
            xml::escape(&self.container),
            xml::escape(&self.instance),
        )
    }
}

/// One request on a connection of its own, by `deadline`.
fn ask<N: Network>(
    network: &mut N,
    request: &Request<'_>,
    deadline: u64,
) -> Result<Response, HttpError> {
    let mut stream = network.connect(ADDRESS, PORT, deadline)?;
    http::exchange(&mut stream, request, deadline)
}

/// One attempt: the goal state, then the `Health` document, both by `deadline`. Why it was
/// refused, in words, otherwise.
fn attempt<N: Network>(network: &mut N, deadline: u64) -> Result<(), String> {
    let said = |request: &Request<'_>, what: &dyn core::fmt::Display| {
        format!("{} {}: {what}", request.method(), request.path())
    };
    let goal = ask(network, &GOAL_STATE, deadline).map_err(|error| said(&GOAL_STATE, &error))?;
    if goal.status != 200 {
        let status = format!("answered {}", goal.status);
        return Err(said(&GOAL_STATE, &status));
    }
    let state = GoalState::parse(&goal.body)
        .map_err(|why| said(&GOAL_STATE, &format!("the goal state {why}")))?;
    let health = state.health();
    let post = Request::post(HOST, HEALTH_PATH, HEALTH_HEADERS, health.as_bytes());
    let answer = ask(network, &post, deadline).map_err(|error| said(&post, &error))?;
    if answer.status != 200 {
        let status = format!("answered {}", answer.status);
        return Err(said(&post, &status));
    }
    Ok(())
}

/// Reports the machine ready to Azure, trying again every [`RETRY_NS`] until it is taken or
/// [`LIMIT_NS`] have passed since the first attempt began.
pub fn report<N: Network, C: Clock>(network: &mut N, clock: &mut C) -> Result<(), Fatal> {
    let started = clock.now();
    let limit = started.saturating_add(LIMIT_NS);
    let mut begins = started;
    let mut last = String::from("no attempt was made");
    loop {
        clock.sleep_until(begins.min(limit));
        let begun = clock.now();
        if begun >= limit {
            return Err(Fatal::new(
                Stage::Platform,
                format!(
                    "Azure did not take the ready report within {} s: {last}",
                    LIMIT_NS / SECOND
                ),
            ));
        }
        begins = begun.saturating_add(RETRY_NS);
        match attempt(network, begins.min(limit)) {
            Ok(()) => return Ok(()),
            Err(why) => last = why,
        }
    }
}
