# ADK model seam

This milestone has no model call. Deterministic Probe/BM25 evidence runs first and fails closed when it cannot verify a source span.

The next semantic slice must consume `adk-workflow-kit` primitives rather than introduce another model client:

1. Capture one immutable route-chain snapshot at invocation start.
2. Admit only explicitly authorized local routes before binding an `OpenAiCompatibleProfile`.
3. Pass the same absolute deadline and cancellation state to every ordered route attempt.
4. Accept only structured output whose cited paths and lines pass the same source verifier.
5. Keep the last valid route configuration when a reload is invalid; never alter an in-flight snapshot.

Live kit inspection shows `OpenAiCompatibleProfile`/`ModelBinding` and development package hot reload, but no ordered model-route chain or model-configuration watcher. The exact dependency seam is therefore a route-chain snapshot plus atomic config publisher around the existing profile binding; no kit source is copied or edited here.
