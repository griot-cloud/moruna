//! The minimal HTTP/1.1 client (`moruna-node-http`) on the recorded answers of the clouds.

use std::vec::Vec;

use super::recorded;
use crate::{
    http::{Header, HttpError, Request, Response, exchange},
    machine::{Stream, StreamError},
};

/// A connection whose server's bytes are written beforehand.
struct Wire {
    /// What the server sends.
    answer: Vec<u8>,
    given: usize,
    /// The most bytes one read gives or one write takes.
    chunk: usize,
    /// What the client sent.
    sent: Vec<u8>,
    /// How the connection ends once the answer is given: closed, or broken.
    end: Option<StreamError>,
    /// The deadline of every call.
    deadlines: Vec<u64>,
}

impl Wire {
    fn new(answer: impl Into<Vec<u8>>) -> Wire {
        Wire {
            answer: answer.into(),
            given: 0,
            chunk: usize::MAX,
            sent: Vec::new(),
            end: None,
            deadlines: Vec::new(),
        }
    }
}

impl Stream for Wire {
    fn write(&mut self, bytes: &[u8], deadline: u64) -> Result<usize, StreamError> {
        self.deadlines.push(deadline);
        let taken = bytes.len().min(self.chunk);
        self.sent.extend_from_slice(&bytes[..taken]);
        Ok(taken)
    }

    fn read(&mut self, buffer: &mut [u8], deadline: u64) -> Result<usize, StreamError> {
        self.deadlines.push(deadline);
        let left = &self.answer[self.given..];
        if left.is_empty() {
            return self.end.clone().map_or(Ok(0), Err);
        }
        let count = left.len().min(buffer.len()).min(self.chunk);
        buffer[..count].copy_from_slice(&left[..count]);
        self.given += count;
        Ok(count)
    }
}

const GET: Request<'static> = Request::get("169.254.169.254", "/latest", &[]);

fn get(answer: impl Into<Vec<u8>>) -> Result<Response, HttpError> {
    exchange(&mut Wire::new(answer), &GET, 90)
}

#[test]
fn the_recorded_answers_are_read() {
    for (answer, status, length) in [
        (recorded::openstack_meta_data(), 200, 427),
        (recorded::openstack_user_data(), 200, 82),
        (recorded::openstack_no_user_data(), 404, 154),
        (recorded::openstack_azure_path(), 404, 154),
        (recorded::azure_instance(), 200, 1683),
        (recorded::azure_no_header(), 400, 66),
        (recorded::azure_unknown_path(), 404, 24),
    ] {
        let response = get(answer).unwrap();
        assert_eq!(response.status, status);
        assert_eq!(response.body.len(), length);
        assert!(matches!(response.header("Date"), Header::One(_)));
    }
}
