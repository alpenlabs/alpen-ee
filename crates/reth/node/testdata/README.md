# Engine Payload Golden Fixture

`engine_payload.bin` pins the byte encoding of `AlpenBuiltPayload` as an `EnginePayload`.

The node stores every block payload in this form (`ExecBlockPayloadSchema`) and replays the stored payloads into reth on restart. A change to the encoding makes existing databases unreadable.

The fixture was written by the codec before it moved out of `alpen-common`. It encodes the payload `fixture_payload` builds in `src/engine_payload.rs`.

Updating the fixture means the stored format changed on purpose, which also needs a migration for existing databases.
