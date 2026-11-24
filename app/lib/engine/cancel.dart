/// Client-side cancellation of requests.
library;

/// Lets the caller of a request say that the answer is no longer needed.
///
/// The protocol has no way to cancel a request (only jobs, with
/// `job_cancel`), and the engine handles requests strictly one after the
/// other. Cancelling therefore works on the client side: a request that is
/// still waiting in the client's queue is never sent, and the answer to one
/// that was already sent is dropped. Either way its future completes at once
/// with a [RequestCancelledException].
///
/// One token can cover several requests; cancelling it cancels all of them.
final class CancelToken {
  bool _cancelled = false;
  final List<void Function()> _listeners = [];

  bool get isCancelled => _cancelled;

  /// Cancels every request made with this token. Idempotent.
  void cancel() {
    if (_cancelled) return;
    _cancelled = true;
    final listeners = List.of(_listeners);
    _listeners.clear();
    for (final listener in listeners) {
      listener();
    }
  }

  /// Calls [listener] when the token is cancelled (at once if it already is).
  void addListener(void Function() listener) {
    if (_cancelled) {
      listener();
    } else {
      _listeners.add(listener);
    }
  }

  void removeListener(void Function() listener) {
    _listeners.remove(listener);
  }
}

/// The outcome of a request whose [CancelToken] was cancelled. Not an error
/// to show: the caller asked for it.
final class RequestCancelledException implements Exception {
  const RequestCancelledException(this.command);

  /// The command that was cancelled.
  final String command;

  @override
  String toString() => 'RequestCancelledException($command)';
}
