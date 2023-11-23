# juliet networking protocol

Version: 1.0.0

## Abstract

This document describes the proposed implementation for a backpressuring, multiplexing communication protocol based on a request-response pattern sent over an underlying reliable streaming protocol. The protocol itself emphasizes easy verifiability and avoidance of denial-of-service opportunities for both participants by putting the rate of processed requests firmly in the hands of the serving side.

### Conventions

The key words "MUST", "MUST NOT", "REQUIRED", "SHALL", "SHALL NOT", "SHOULD", "SHOULD NOT", "RECOMMENDED",  "MAY", and "OPTIONAL" in this document are to be interpreted as described in [RFC 2119](https://www.ietf.org/rfc/rfc2119.txt).

A violation of any "MUST", "MUST NOT", "REQUIRED", "SHALL" or "SHALL NOT" specification in this document should be assumed to have profound security implications and is to be avoided by any implementer.

## Versioning and changelog

This protocol description follows the [Semantiv Versioning 2.0.0](https://semver.org/) scheme, adapted for describing a protocol. Specifically,

* MAJOR version differences indicate incompatible protocol versions,
* MINOR version differences indicate additional optional features that are backwards and forwards compatible with any compliant implementation of a previous protocol version, while
* PATCH version differences are clarifications/changes in wording or extensions to just the document itself that should have no impact on implementations.

### Unreleased

* Removed outdated information from footer of RFC.

### 1.0.0

Initial release.

## Introduction

During implementation of the casper-blockchain, an apparent need for multiple subcomponents of the software to communicate simultaneously became apparent, as a typical proof-of-stake blockchain shares characteristics with both large peer-to-peer file sharing applications as well as time-critical distributed databases. More concrete, consensus protocols have soft real-time requirements, failure to meet them resulting in a financial loss for the operator. At the same time keeping the network accessible to new clients requires sending large amounts of historical data to these peers whilst being unavoidable to support a growing overall network.

Additionally there is financial incentive for malicious third parties to disrupt the network  through various means, including denial-of-software attacks, e.g. by overloading a node of the network through a large number of requests.

The juliet networking protocol intends to address some of these requirements. It provides a way for two peers to make multiplexed parallel requests, with no peer being able to stress its counterpart in terms of bandwidth, CPU or memory usage over a pre-defined limit within the bounds of their connection.

## Prerequisites

### Transport

The juliet protocol is defined on top of a **reliable**, **ordered** and **error checked** packet- or stream-based protocol. In other words, data should eventually arrive, in the order it was sent, without errors, or the connection be terminated with an error instead.

Since every packet is either of predetermined length or length-prefixed, separating packets that are being sent as a stream is possible, thus there is no large difference between stream- and packet based protocols as far as juliet is concerned, as what would be invididual packets can be reconstructed from any input stream.

Typical implementations will be most often based on stream protocols like TCP or TLS and thus assumed to be the default case in this document. Precisely specifying the protocol over packet-based protocols is done through additional remarks.

### Terminology

* **Payload**: A bytestring of user data, opaque to the juliet protocol.
* **Varint32**: A variable size encoded 32 bit integer, taking up to 5 bytes in size.
* **Frame**: The smallest unit that is processed at once. Combines a header with an optional segment.
* **Header**: Start of a frame, 4 bytes in length.
* **Frame kind**: The kind field of a header.
* **Segment**: The optional portion following a header. Segments may be used to transfer payloads longer than what fits into a single frame, resulting in three different segment types (start, intermediate and end segment).
* **Start segment**: A segment prefixed with a varint32, followed by payload data.
* **Intermediate segment**: A segment without a size prefix, following a start segment. It always contains the maximum amount of payload bytes permissible in the segment as determined by `MAX_FRAME_SIZE`.
* **End segment**: The final segment in a sequence of segments. Does not contain a size prefix, but can be shorter than the maximum amount size determined by `MAX_FRAME_SIZE`.
* **Channel**: A logical message channel, numbered from `0` to `255`.
* **Valid channel**: A channel with a number less than `NUM_CHANNELS` (see constant section below).
* **ID**: A temporarily unique 16 bit identifier.
* **Message**: One of the message types described below, assembled from one or more frames.
* **Sender**: A peer that is sending data.
* **Receiver**: A peer that is sent data.
* **Request**: An initial message sent by one of the peers with one of the request message types.
* **Response**: The answer to a request. Must match its channel and ID and be a response message type.
* **In-flight request**: A request which has not been fully processed by the receiver yet.
* **Incomplete request**: A request spanning multiple frames for which not all frames have been received yet.

## Frame structure

Each frame consists of a header, followed by an optional segment. Every header is exactly four bytes in size. A segment follows if the extracted error or frame kind dictates it.

```
          Frame
┌───────────┬────────────────────────────────┐
│ Header    │ Segment                        │
└───────────┴────────────────────────────────┘
 00 01 02 03 (04 05 06 07 .. MAX_FRAME_SIZE-1)
```

### Headers

The four byte header has three fields, the kind byte, channel byte and ID. Kind is a single byte that indicates the frame type. Channel is an unsigned single byte integer. ID is a 16-bit integer taking up two bytes, in little endian encoding; thus the least significant byte is at position `02` and the most significant byte in position `03` in the header.

```
         Header
┌──────────┬──────────┬──────────────────────┐
│ Kind     │ Channel  │ ID (lsb)  ID (msb)   │
└──────────┴──────────┴──────────────────────┘
 00         01         02         03
```

The kind byte's highest bit `7` is called the `ERROR` bit. If it is set, an error is encoded in `DATA0` through `DATA3`, otherwise a frame kind is encoded in `DATA0` through `DATA2`. Bits `4` through `6` are reserved for future use.

```
┌──────────┬──────────┬──────────┬──────────┐
│ ERROR    │ RESERVED │ RESERVED │ RESERVED │
└──────────┴──────────┴──────────┴──────────┘
 7          6          5          4       ..
┌──────────┬──────────┬──────────┬──────────┐
│ DATA3    │ DATA2    │ DATA1    │ DATA0    │
└──────────┴──────────┴──────────┴──────────┘
 ..3        2          1          0 (lsb)
```

To form the error or kind number from `DATA` bits, it is sufficient to zero out all non-data bits (bits `3` through `7` in the case of `ERROR` being 0, bits `4` through `7` otherwise). The resulting byte can be interpreted as an integer directly after.

#### Frame/message kinds

A frame's kind is dependent from the kind number, as described below. Message kinds are identical to frame kinds. A frame has a segment if and only if the matching message kind has a payload.

| Kind number | Message kind | Has payload | Meaning     |
| ----------- | ------------ | ----------- | ----------- |
|           0 | `REQUEST`     | no         | A request |
|           1 | `RESPONSE`    | no         | A response |
|           2 | `REQUEST_PL`  | yes        | Request with a payload |
|           3 | `RESPONSE_PL` | yes        | Response with a payload |
|           4 | `CANCEL_REQ`  | no         | Request cancellation |
|           5 | `CANCEL_RESP` | no         | Response cancellation |

### Varint32

A varint32 is an unsigned 32 bit integer encoded with a variable length, which encodes 7 bit per byte of the value in little endian byte order, with the highest order bit indicating that more bits will follow.

To encode a 32 bit integer `n`, the following algorithm is used:

* Let `M` be an 8-bit bitmask of all `1`s except the highest bit (`0`), i.e. `01111111` in binary / `127` decimal,
* `H` the inverse of `M`, i.e. the value `10000000` in binary / `128` in decimal,
* `n[0]` the lowest significant byte of `n`,
* `>>` a bitwise right shift (non-rotating),
* `<<` a bitwise left shift (non-rotating),
* `&` a bitwise AND operation, and
* `|` a bitwise OR operation.

Encode as follows:

1. Set `k` = `n[0] & M`.
2. Set `n` = `n >> 7`.
3. If `n` > 0, set `k` = `k | H`.
4. Output `k` as the next byte.
5. If `n` == 0, finish.
6. Go to 1.

Decoding can be done as follows:

1. Set `n` = 0, `i` = 0.
2. Consume one byte from the input as `k`. If there is no more input, return an error indicating insufficient data.
3. If `i` >= `4` && `k & H` != `0`, return an error.
4. Set `n` = `n | ((k & M) << (i*7))`.
5. If `k & H` == `0` return `n`.
6. Set `i = i + 1`.
7. Go to 2.

Example values:

| Integer value (decimal) | little endian 32-bit integer | `varint32`       |
| ----------------------- | ---------------------------- | ---------------- |
| `0`                     | `00 00 00 00`                | `00`             |
| `64`                    | `40 00 00 00`                | `40`             |
| `127`                   | `7F 00 00 00`                | `7F`             |
| `128`                   | `80 00 00 00`                | `80 01`          |
| `255`                   | `FF 00 00 00`                | `FF 01`          |
| `65535`                 | `FF FF 00 00`                | `FF FF 03`       |
| `305419896`             | `78 56 34 12`                | `F8 AC D1 91 01` |
| `4294967295`            | `FF FF FF FF`                | `FF FF FF FF 0F` |

### Segments

Segments are the data section of a frame, always used to send payloads. Segment data is always length prefixed, indicating the length of the total payload (without headers), which may be reassembled from multiple segments.

A frame with a header whose associated message kind dictates a payload MUST contain a segment. Otherwise it MUST NOT contain a segment. Peers finding their partners in violation must send a `SEGMENT_VIOLATION` error.

A start segment begins with the varint32 encoded length of the entire payload, which MUST be followed by as much payload data as possible, filling the frame size. A frame is filled when its total size including the header is equal to `MAX_FRAME_SIZE`.

The order and size of all frames is determined by the length of the payload. If a frame containing a start segment is enough to contain the entire payload, no additional frames are sent to transfer this payload. If additional data needs to be transferred, for every additional `MAX_FRAME_SIZE - 4` bytes, an intermediate frame must be sent. Once less than `MAX_FRAME_SIZE - 4` bytes are left to transfer, an end frame is sent. If there are 0 bytes left after the last intermediate frame, the end frame is omitted. A peer found in violation of any of these rules MUST be sent a `SEGMENT_VIOLATION` error or another error stemming from the potential misinterpretation of the following data.

```
        Frame with start segment
┌───────────┬───────────────────┬────────────┐
│ Header    │ VSlen ..          │ data       │
└───────────┴───────────────────┴────────────┘
 00 01 02 03 04 05? 06? 07? 08?  ..
```

```
       Frame with intermediate segment
┌───────────┬────────────────────────────────┐
│ Header    │ data                           │
└───────────┴────────────────────────────────┘
 00 01 02 03 04 ..           MAX_FRAME_SIZE-1
```

```
       Frame with end segment
┌───────────┬────────────────────────────────┐
│ Header    │ data                           │
└───────────┴────────────────────────────────┘
 00 01 02 03 04 (05 06 07 .. MAX_FRAME_SIZE-1)
```

#### Determining the number of frames for a given segment

We can define a function `C(n)` that determines the number of segments (and thus frames) required for sending a payload of size `n >= 0`. Let `VL(k)` be the number of bytes required to varint32-encode an integer `k`, `//` integer division and `cdiv(n, k) = (n + k - 1) // k`. Then

```
C(n) = 1 + cdiv(MAX(0, n+4+VL(n)-MAX_FRAME_SIZE), (MAX_FRAME_SIZE-4))
```

## Protocol flow

### Initial state and constants

The juliet protocol is built on top of a reliable stream- or packet-based protocol, thus the initial stage assumes both peers are ready to send and receive.

Some constants have to be set and distributed to both peers beforehand, this can be done using an external negotiation scheme or just prior definition. Since the values of these constants are application specific, only a recommendation for their range is given here.

Some of these limits are channel specific, denoted by the `_n` suffix, indicating that they can be set on a per-channel basis. As an example, `REQUEST_LIMIT_5` is the in-flight request limit for channel 5.

* `MAX_FRAME_SIZE` is the maximum size for a single frame, including header and segment. For implementations, this indicates the minimum memory required to receive a single frame. The recommended default value is 4 KB.
* `NUM_CHANNELS` denotes the total number of channels where data can be sent on. No default value can be recommended. A valid channel number `c` is bound by `0 < c < n`.
* `REQUEST_LIMIT_n` is the maximum number of in-flight requests on channel `n`. An optimal value is application specific, but a value of 1 is always a safe bet. The maximum possible value for `REQUEST_LIMIT_n` is 65535, since IDs are 16 bit integers and must not collide.
* `MAX_REQUEST_PAYLOAD_SIZE_n` is the maximum payload allowed for a message of kind `REQUEST_PL` on channel `n`.
* `MAX_RESPONSE_PAYLOAD_SIZE_n` is the maximum payload allowed for a message of kind `RESPONSE_PL` on channel `n`.

There are two channels states for every valid channel named `READY` and `RECEIVING`. Every channel `n` carries the state as `STATE_n` and following state information on both peers:

* `INCOMING_REQS_n`: A set of IDs that are currently considered requests in flight received by the local peer.
* `OUTGOING_REQS_n`: A set of IDs that sent by the local peer which are in flight.
* `CANCELLATION_ALLOWANCE_n`: An integer with a range between `0` and `REQUEST_LIMIT_n` with initial value `0`.

The `READY` state of a channel carries no additional fields, while the `RECEIVING` state records a current ID, data received so far and total payload size.

All valid channels must be initialized to the `READY` state, and empty `INCOMING_REQS_n` and `OUTGOING_REQS_n` sets.

### Sending frames

A peer becomes a sender by sending a frame, which MUST NOT exceed `MAX_FRAME_SIZE`. If a sender sends a frame exceeding `MAX_FRAME_SIZE`, the receiver MUST send a `MAX_FRAME_SIZE_EXCEEDED` error (see below for how to send errors).

The receiver then decodes the frame header, which will yield either an error due to an invalid kind number, or frame/error kind (see the frame structure section for details). If no valid of either one can be decoded, the receiver MUST send an `INVALID_HEADER` error.

The follow-up action of a receiver of a frame depends on the frame kind sent. See below detailed message flows.

### Sending errors

A peer sending an error to the other peer sends the error like a regular frame. It SHOULD send it at the earliest opportunity. Errors are encoded using by setting the error bit in the kind byte of the header and encoding the 4-bit error number in the header. There are 16 possible error numbers, not all of which are in use.

Once an error has been sent, the sender SHOULD attempt to flush the outgoing connection. Both sender and receiver of an error MUST close the connection immediately afterwards, regardless of which channel the error was received on.

Errors are only sent in response to received messages, with the exception of `OTHER`. An error sent MUST mirror the the channel and ID of the message that caused it.

An error of kind `OTHER` must include a payload, but may never be a multi-frame message; it is thus limited to whatever fits into the remainder of the frame after header and encoded length. The receiver of an `OTHER` error that would start a multi-frame reception MUST NOT accept it and SHOULD send a `SEGMENT_VIOLATION` error back before closing the connection.

If an error with an invalid error number is received, the receiver MUST close the connection and MUST NOT send an `INVALID_HEADER` error back.

| Err no. | Name                          | Error |
| ------- | ----------------------------- | ----- |
|       0 | `OTHER`                       | Application-specific error, MUST include a payload |
|       1 | `MAX_FRAME_SIZE_EXCEEDED`     | `MAX_FRAME_SIZE` has been exceeded, cannot occur in stream based implementations |
|       2 | `INVALID_HEADER`              | The header was not understood |
|       3 | `SEGMENT_VIOLATION`           | A segment was sent with a frame where none was allowed, or a segment was too small or missing, or an error payload too large |
|       4 | `BAD_VARINT`                  | A varint32 could not be decoded |
|       5 | `INVALID_CHANNEL`             | Invalid channel: A channel number greater or equal `NUM_CHANNELS` was received |
|       6 | `IN_PROGRESS`                 | A new request or response was sent without completing the previous one |
|       7 | `RESPONSE_TOO_LARGE`          | `MAX_RESPONSE_PAYLOAD_n` would be exceeded by advertised payload |
|       8 | `REQUEST_TOO_LARGE`           | `MAX_REQUEST_PAYLOAD_SIZE_n` would be exceeded |
|       9 | `DUPLICATE_REQUEST`           | A sender attempted to create two in-flight requests with the same ID on the same channel |
|      10 | `FICTITIOUS_REQUEST`          | Sent a response for request not in-flight |
|      11 | `REQUEST_LIMIT_EXCEEDED`      | `REQUEST_LIMIT_n` for channel `n` exceeded |
|      12 | `FICTITIOUS_CANCEL`           | Sent a response cancellation for request not in-flight |
|      13 | `CANCELLATION_LIMIT_EXCEEDED` | Sent a request cancellation exceeding the cancellation allowance |

### Channel states and message flow

To send a frame on channel `n`, peer MUST set the channel field of the header to `n`. A peer MUST NOT send any non-error frame on a channel that is not valid. A receiving peer MUST NOT accept any non-error frame header for a non-valid channel, instead it MUST send back an `INVALID_CHANNEL` error, which MUST be sent on the invalid channel that it was received on.

Beyond sending errors, which can be done by both peers at any time, a sending peer can initiate the following flows on any valid channel:

#### Sending a request without a payload

To send a request without a payload, a sender MUST create a header with kind `REQUEST`, channel `n` and an unused ID (see below for details) and send it.

The receiver of a frame of kind `REQUEST` MUST check if `INCOMING_REQS_n` is greater or equal in size than `REQUEST_LIMIT_n`; if it is the receiver MUST send back a `REQUEST_LIMIT_EXCEEDED` error.

The receiver MUST track the incoming request ID by adding it to `INCOMING_REQS_n` and increase `CANCELLATION_ALLOWANCE_n` by one, unless it is already at its maximum value. If `INCOMING_REQS_n` already contained the given ID, the receiver MUST send back a `DUPLICATE_REQUEST` error. It now yields a valid `REQUEST` message to the application.

#### Sending a request with a payload

To begin sending a request message with a payload of length `p` on channel `n`, a sender MUST determine whether the payload fits a single starting segment by calculating the required number of frames for the payload (`C(p)`) and checking if it equals 1. It MUST NOT send any messages with `p` exceeding `MAX_REQUEST_PAYLOAD_SIZE_n`.

The sender of a request with a payload MUST create a header with kind `REQUEST_PL`, channel `n` and an unused ID (see below for details). It sends one, or multiple as specified by the "Transferring multi-frame messages" frames to the receiver.

The receiver of a frame of kind `REQUEST_PL` with a starting segment MUST check if `INCOMING_REQS_n` is greater or equal in size than `REQUEST_LIMIT_n`; if it is the receiver MUST send back a `REQUEST_LIMIT_EXCEEDED` error. If the advertised size of the payload exceeds `MAX_REQUEST_PAYLOAD_SIZE_n`, it MUST send back a `REQUEST_TOO_LARGE` error to the sender.

The receiver MUST track the incoming request ID by adding it to `INCOMING_REQS_n` on reception of the starting segment and increase `CANCELLATION_ALLOWANCE_n` by one, unless it is already at its maximum value. If `INCOMING_REQS_n` already contained the given ID, the receiver MUST send back a `DUPLICATE_REQUEST` error.

Once the ending segment was received, the receiver yields a valid `REQUEST_PL` message to the application.

#### Sending a response without a payload

To send a response without a payload, a sender MUST create a header with kind `RESPONSE`, channel `n` and the ID of a currently in-flight request from `INCOMING_REQS_n`. Once the response has been sent, the sender removes the ID from `INCOMING_REQS_n`.

The receiver of a frame of kind `RESPONSE` MUST check if `OUTGOING_REQS_n` contains the request, if not it must send a `FICTITIOUS_REQUEST` error to the sender. Otherwise it removes the ID from `OUTGOING_REQS_n` and yields a valid `RESPONSE` message to the application.

#### Sending a response with a payload

To begin sending a response message with a payload of length `p` on channel `n`, a sender MUST determine whether the payload fits a single starting segment by calculating the required number of frames for the payload (`C(p)`) and checking if it equals 1. It MUST NOT send any messages with `p` exceeding `MAX_RESPONSE_PAYLOAD_SIZE_n`.

To send a response without a payload, a sender MUST create a header with kind `RESPONSE_PL`, channel `n` and the ID of a currently in-flight request from `INCOMING_REQS_n`. It sends one, or multiple as specified by the "Transferring multi-frame messages" frames to the receiver. After the first frame has been sent, the sender removes the ID from `INCOMING_REQS_n`.

The receiver of a frame of kind `RESPONSE_PL` with a starting segment MUST check if `OUTGOING_REQS_n` contains the request, if not it must send a `FICTITIOUS_REQUEST` error to the sender. Otherwise it removes the ID from `OUTGOING_REQS_n` and yields a valid `RESPONSE_PL` message to the application.

#### Sending a request cancellation

To send a request cancellation, a sender MUST create a header with kind `CANCEL_REQUEST`, channel `n` and the ID of a currently in-flight request from `OUTGOING_REQS_n`. The sender MUST NOT remove this ID from `OUTGOING_REQS_n` until it has received either a response or response cancellation with the ID. It SHOULD never send more than one response cancellation for a given ID on a channel.

The receiver of a request cancellation MUST decrement `CANCELLATION_ALLOWANCE_n` by one, or if it already is `0`, send a `CANCELLATION_LIMIT_EXCEEDED` error back to the sender.

It then yields a valid `REQUEST_CANCELLATION` message to the application.

#### Sending a response cancellation

To send a response cancellation, a sender MUST create a header with kind `CANCEL_RESPONSE`, channel `n` and the ID of a currently in-flight request from `INCOMING_REQS_n`. Once the cancellation has been sent, the sender removes the ID from `INCOMING_REQS_n`.

The receiver of a frame of kind `CANCEL_RESPONSE` MUST check if `OUTGOING_REQS_n` contains the request, if not it must send a `FICTITIOUS_CANCEL` error to the sender. Otherwise it removes the ID from `OUTGOING_REQS_n` and yields a valid `CANCEL_RESPONSE` message to the application.

### Obtaining an unused ID on a given channel

The sender of a new request on channel `n` MUST obtain an unused ID each time, according to the following process:

The sender MUST first check its `OUTGOING_REQS_n` set - if it is equal or greater in size than `REQUEST_LIMIT_n`, it MUST delay sending the entire request until `OUTGOING_REQS_n` has shrunk in size.

The sender than MUST select any unsigned 16 bit integer that is not in `OUTGOING_REQS_n` as the unused ID.

### Transferring multi-frame messages

A message sent with a payload that is too large to fit into a single segment MUST be sent as a multi-frame message. The split of the payload across multiple frames MUST be done as described in the "Segments" section of this document.

Before beginning to send a multi-frame message on channel `n`, a sender MUST first ensure that it is not currently sending any other multi-frame message on the same channel. Note that this means that only a multi-frame request or response can be sent on a channel by the same peer, but not both at the same time. If there already is a multi-frame send in progress, the sender MUST delay the entire sending process of following multi-frame messages until it has concluded.

A peer sending a multi-frame message MUST NOT send segments out of order, it MUST initiate the send by sending a frame with a starting segment. All following frames that are part of the multi-frame message MUST be frames with intermediate segments, except for the last one, which MUST be a frame with an end segment.

The sending peer MAY send any number of non-multi-frame messages in between an active multi-frame message transfer, including messages with a payload that is not multi-frame due to its small enough size. It MUST NOT begin any other multi-frame message transfers while a send of its own is incomplete, thus at most one multi-frame message transfer is active per sending peer on a channel `n` at any one time.

A sending peer SHOULD eventually complete the multi-frame message send, but a receiving peer MUST NOT rely on this invariant being upheld for any of its own soundness or security.

A multi-frame message transfer MUST begin with a header kind suitable for multi-frame messages. All subsequent frames of the multi-frame message send MUST repeat exactly the same header.

The receiver of of a multi-frame transfer, upon receiving a frame, if the frame has no segment, MUST send back a `SEGMENT_VIOLATION` error. Otherwise the receiver acts according to its `STATE_n`:

If `STATE_n` is in the `READY` state the receiver MUST attempt to decode the varint32 from the starting segment to be interpreted as the payload length. If decoding fails, it MUST send back a `BAD_VARINT` error. The receiver MUST set `STATE_n` to `RECEIVING`, storing the ID, payload size, and allocate a buffer with the received payload data included in the segment, but SHOULD NOT preallocate space for the entire payload.

If `STATE_n` is in the `RECEIVING` state with an ID that does not match the newly received frames ID, it MUST send an `IN_PROGRESS` error.

If `STATE_n` is `RECEIVING` with an ID that does match the newly received frames ID,
it adds the received data to its buffer, unless the newly received data would cause it to exceed the advertised payload size, in which case it MUST send back a `SEGMENT_VIOLATION` to the sender. If the received frame is expected to contain an intermediate segment but is not of `MAX_FRAME_SIZE` length, the receiver MUST send back a `SEGMENT_VIOLATION` to the sender. If the received frame contains the expected end segment, the complete payload is yielded and `STATE_n` reset to the `READY` state.

### Stream-based transports

In a stream-based transport, which has no implicit separation, frames can still be read, as the length of the next slice of bytes required is always known. A peer reading frames SHOULD always follow the following algorithm:

1. Consume 4 bytes, decoding the header.
2. If a segment is expected, if it is the first, consume single bytes until the varint32 length has been read.
3. Consume the now known remainder of the segment, if any.

This means that the `MAX_FRAME_SIZE_EXCEEDED`  and `SEGMENT_VIOLATION` errors cannot occur, as the exceeding/missing bytes will simply be interpreted as belonging to /taken from the following frame.

## Application level concerns

### Heartbeats

The core juliet protocol deliberatly avoids any timing concerns, e.g. keep-alive pings or heartbeats. On the application level, a recommended algorithm is:

1. Define a `KEEPALIVE` timeout (e.g. 30 seconds).
2. Dedicate a channel to "heartbeat" messages.
3. Send a "heartbeat" message every half duration of `KEEPALIVE`.
4. When receiving a "heartbeat" message, mark its receival. If it is earlier than one quarter of the `KEEPALIVE` timeout after another "heartbeat", send an application error and disconnect.
5. If `KEEPALIVE` elapsed without any data being received, disconnect.

### Timeouts

Like heartbeats, timeouts are out of scope for the core protocol, but an implementation may consider adding built-in support for these. As applications using the juliet protocol will already have to address potential response cancellations by remote peers, an implementation can support callers specifying a timeout for every request by tracking these, then sending an implicit cancellation to the peer and yielding a synthesized response cancellation to the application.
