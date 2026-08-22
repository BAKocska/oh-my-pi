# Durable campaign retry

This extension declares a Session-scoped campaign that persists a typed `RetryState`, continues two settled turns, then emits `Force("write")` on turn three.

The declaration is sealed during FREEZE. Its state envelope is `examples.campaign-retry.state@1`, so replacing the extension-host process restores the same rung and turn count from the journal. If the schema revision changes, Core exhausts the old engagement instead of loading incompatible state.

Run it in a session, engage it with `await omp.campaigns.engage("three-turn-retry", state=RetryState())`, and settle three turns. A simultaneous core-native force is serialized through the `tool_choice` claim queue; the losing force remains queued without consuming its ladder rung.
