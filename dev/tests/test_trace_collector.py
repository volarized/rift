"""Loopback acceptance recovery for the development OTLP collector."""

from __future__ import annotations

import asyncio
import socket
import sys
import unittest
from unittest.mock import AsyncMock, patch
from urllib.request import Request, urlopen

from rift_dev import trace


def _windows_accept_error(code: int) -> OSError:
    error = OSError("simulated Windows accept error")
    error.winerror = code
    return error


class AcceptRetryTests(unittest.IsolatedAsyncioTestCase):
    async def test_one_transient_disconnect_can_retry(self) -> None:
        accepted = object()
        accept = AsyncMock(
            side_effect=[
                _windows_accept_error(trace.WINDOWS_ERROR_NETNAME_DELETED),
                accepted,
            ]
        )
        with socket.socket() as listener:
            self.assertIs(
                await trace._accept_after_windows_disconnect(accept, listener), accepted
            )
        self.assertEqual(accept.await_count, 2)

    async def test_other_errors_and_cancellation_leave_accept_unchanged(self) -> None:
        for error in (_windows_accept_error(5), asyncio.CancelledError()):
            with self.subTest(error=type(error)), socket.socket() as listener:
                accept = AsyncMock(side_effect=error)
                with self.assertRaises(type(error)) as failure:
                    await trace._accept_after_windows_disconnect(accept, listener)
                self.assertIs(failure.exception, error)
                self.assertEqual(accept.await_count, 1)

    async def test_repeated_disconnect_or_closed_listener_is_not_retried_forever(
        self,
    ) -> None:
        for close, attempts in ((False, 2), (True, 1)):
            with self.subTest(close=close), socket.socket() as listener:
                if close:
                    listener.close()
                error = _windows_accept_error(trace.WINDOWS_ERROR_NETNAME_DELETED)
                accept = AsyncMock(side_effect=error)
                with self.assertRaises(OSError) as failure:
                    await trace._accept_after_windows_disconnect(accept, listener)
                self.assertIs(failure.exception, error)
                self.assertEqual(accept.await_count, attempts)


if sys.platform == "win32":

    class WindowsCollectorTests(unittest.TestCase):
        def test_following_export_reaches_the_same_listener_after_a_disconnect(
            self,
        ) -> None:
            original_accept = asyncio.IocpProactor.accept
            listeners: list[socket.socket] = []

            def interrupt_accept(
                proactor: asyncio.IocpProactor, listener: socket.socket
            ):
                listeners.append(listener)
                if len(listeners) == 1:
                    result = asyncio.get_running_loop().create_future()
                    result.set_exception(
                        _windows_accept_error(trace.WINDOWS_ERROR_NETNAME_DELETED)
                    )
                    return result
                return original_accept(proactor, listener)

            with (
                patch.object(asyncio.IocpProactor, "accept", interrupt_accept),
                trace.collector() as running,
            ):
                request = Request(
                    running.endpoint + trace.METRICS_PATH,
                    data=b"",
                    headers={"Content-Type": trace.PROTOBUF},
                )
                with urlopen(request, timeout=2) as response:
                    self.assertEqual(response.status, 200)
                    response.read()
                self.assertEqual(running.requests.received, 1)
                self.assertGreaterEqual(len(listeners), 2)
                self.assertIs(listeners[0], listeners[1])
