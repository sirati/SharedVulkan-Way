"""Small lifecycle corrections for the pinned dbus-next 0.2.3 service runtime.

Upstream 0.2.3 sends asynchronous method errors but fails to suppress the same
exception in its reply context, and disconnect shuts down without closing the
socket objects. Keep these fixes local to our service, not Linux applications.
"""
from dbus_next.aio import MessageBus


class SessionBus(MessageBus):
    def _send_reply(self, message):
        reply = super()._send_reply(message)

        class Reply(type(reply)):
            def __exit__(self, exception_type, exception, traceback):
                return self._exit(exception_type, exception, traceback)

        return Reply()

    def disconnect(self):
        if not self._disconnected:
            super().disconnect()

    def _finalize(self, error=None):
        try:
            super()._finalize(error)
        finally:
            self._stream.close()
            self._sock.close()
