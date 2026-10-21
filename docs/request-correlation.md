# Request correlation

Slotline generates a fresh opaque 128-bit ID for each incoming HTTP request. It returns the ID in `X-Request-ID`, replacing any caller-supplied value so arbitrary caller metadata does not become log identity. IDs are correlation metadata, not credentials.

Structured JSON events expose the ID in `span.request_id` (and the request entry of `spans`). Route selection, skips, upstream failures, retries, and successful routing inside the handler share the request span. Body polling is instrumented too, so stream errors after response headers retain the original ID.

## Request lifecycle events

- `request rejected or failed`: response status >= 400, including admission and validation errors. The event reports status, not request content.
- `request response headers`: successful response headers; this does not prove stream completion.
- `request stream failed`: body-consumption error with a safe category, without raw upstream errors, body contents, or secrets.
- `request body completed`: response body exhausted without a body error. An HTTP error body can complete too; inspect its status and preceding events.
- `request body cancelled` and `request handler cancelled`: terminal cancellation observations, emitted when the body or handler is dropped before normal completion. Cancellation can happen before response, before the first body poll, or mid-stream.

Background health/qualification probes are not incoming HTTP requests and do not receive these IDs. Existing route/stage fields identify probe failures. IDs are local correlation metadata, not distributed tracing; Slotline does not currently propagate them to upstream providers. Client cancellation can drop a response body without a completion event. Request/body completion describes server-side polling and does not prove remote receipt or display.

## Verify correlation behavior

Build the binary and set `SLOTLINE_TEST_HOST` to an address assigned to your
test host, as described in [development checks](DEVELOPMENT.md#real-http-fixture-processes).
Then run the fixture from the repository root:

```sh
python3 tests/stream_deadline_validation.py
```

This script checks unique server IDs, replacement of spoofed IDs, rejection correlation, route/stream-error correlation, and primary transport failure followed by fallback success under the same ID against isolated HTTP fixtures. It is a local fixture check, not evidence of production deployment or remote behavior.
