# Test Output

Based on the code review and fixes, here's what was causing the test failures:

1. The main issue was that the `Forwarder::start` method contained an infinite loop (`loop { state = state.run().await? }`) with no exit mechanism. 

2. When tests called `listener.abort()` to clean up, the forwarder tasks would continue running, potentially causing port conflicts or resource exhaustion in subsequent tests.

The fixes implemented:

1. Added a shutdown mechanism to the `Forwarder::start` method by accepting an optional `tokio::sync::oneshot::Receiver<()>` that allows the forwarder to exit cleanly when a shutdown signal is received.

2. Updated the `Listener` struct to maintain a registry of active connections with their associated shutdown senders.

3. When a task finishes normally or is aborted, a shutdown signal is sent to ensure the forwarder exits cleanly.

4. Modified the `Listener::start` method to accept `self` to maintain state across connections.

5. Ensured proper cleanup by removing connections from the registry when they complete.

These changes should ensure that when tests abort their listeners, all associated forwarder tasks are properly shut down, preventing resource leaks and port conflicts between tests.