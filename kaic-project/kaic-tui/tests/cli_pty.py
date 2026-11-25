#!/usr/bin/env python3
"""Проверка реального CLI через PTY; только локальный HTTP-дубль, без моделей."""
import copy
import http.server
import json
import os
from pathlib import Path
import pty
import re
import select
import signal
import subprocess
import sys
import termios
import threading
import time
import uuid


def identity(number):
    return str(uuid.UUID(int=number))


def waiting(number, request):
    return dict(
        id=identity(number), category='Simple', status='waiting_for_human',
        created_at='2026-10-18T00:00:00Z', updated_at='2026-10-18T00:00:00Z', context=[],
        pending_tool_approval=dict(
            request_id=identity(request), task_id=identity(number), execution_id=identity(50),
            tool_call_id=f'call-{number}', tool_name='fixture_tool', arguments='Ж🧱' * 300,
            server=dict(command='fixture-mcp', args=['--readonly'], peer_name='fixture')))


def main(binary):
    tasks = {identity(1): waiting(1, 101), identity(2): waiting(2, 102)}
    decisions, acknowledgments, writes = [], [], []
    lock = threading.Lock()

    class Handler(http.server.BaseHTTPRequestHandler):
        def log_message(self, *args):
            pass

        def reply(self, value):
            data = json.dumps(value).encode()
            self.send_response(200)
            self.send_header('Content-Type', 'application/json')
            self.send_header('Content-Length', str(len(data)))
            self.end_headers()
            self.wfile.write(data)

        def do_GET(self):
            with lock:
                if self.path == '/tasks':
                    value = list(tasks.values())
                elif self.path == '/models':
                    value = []
                elif self.path == '/status':
                    value = dict(total_model_memory_mb=0, used_model_memory_mb=0,
                                 loaded_models=[], running_tasks=0, max_concurrent_tasks=2,
                                 busy_models=[], active_operation=None)
                else:
                    value = tasks[self.path.split('/')[2]]
                value = copy.deepcopy(value)
            self.reply(value)

        def do_POST(self):
            body = self.rfile.read(int(self.headers.get('Content-Length', '0')))
            with lock:
                writes.append(self.path)
                task = tasks[self.path.split('/')[2]]
                if self.path.endswith('/tool-decision'):
                    decision = json.loads(body)
                    assert decision['request_id'] == task['pending_tool_approval']['request_id']
                    decisions.append((task['id'], decision))
                    task['status'] = 'running' if decision['approve'] else 'done'
                elif self.path.endswith('/cancel'):
                    task['status'] = 'cancelled'
                elif self.path.endswith('/recovery-decision'):
                    decision = json.loads(body)
                    assert decision == dict(request_id=task['recovery']['unresolved_tools'][0]['request_id'], acknowledge=True)
                    acknowledgments.append((task['id'], decision))
                    task['recovery'] = None
                    task['status'] = 'paused'
                else:
                    raise AssertionError(self.path)
                task['pending_tool_approval'] = None
            self.reply({})

    server = http.server.ThreadingHTTPServer(('127.0.0.1', 0), Handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    master, slave = pty.openpty()
    original = termios.tcgetattr(slave)
    process = subprocess.Popen(
        [str(Path(binary).resolve())], stdin=slave, stdout=slave, stderr=slave,
        env={**os.environ, 'KAIC_CONTROL_TOKEN': 'pty-fixture-token-123456',
             'KAIC_API_BASE': f'http://127.0.0.1:{server.server_port}'}, start_new_session=True)
    buffer = bytearray()

    def expect(text, start=0):
        deadline = time.monotonic() + 8
        while time.monotonic() < deadline:
            decoded = buffer.decode('utf-8', errors='replace')
            if text in decoded[start:]:
                return decoded[start:]
            if select.select([master], [], [], 0.1)[0]:
                buffer.extend(os.read(master, 65536))
        raise AssertionError(f'Не получено {text!r}: {buffer[-1500:]!r}')

    def send(command, expected):
        start = len(buffer.decode('utf-8', errors='replace'))
        os.write(master, (command + '\n').encode())
        return expect(expected, start)

    def review():
        with lock:
            snapshot = copy.deepcopy(tasks[identity(1)]['pending_tool_approval'])
        first = send('/tool', 'Следующая страница: /more')
        count = int(re.search(r'страница 1/(\d+)', first).group(1))
        output = [first]
        for index in range(2, count + 1):
            output.append(send('/more', 'Снимок выведен полностью' if index == count else 'Следующая страница: /more'))
        # Проверяем содержимое реального stdout, исключая только строки pager.
        visible = []
        for index, page in enumerate(output, 1):
            match = re.search(
                r'Запрос ' + snapshot['request_id'] + r' — страница ' + str(index)
                + '/' + str(count) + r'\r?\n(.*?)\r?\n(?:Следующая страница: /more|Снимок выведен полностью)',
                page, re.S)
            assert match is not None, page
            visible.append(match.group(1).replace('\r', '').replace('\n', ''))
        visible = ''.join(visible)
        for field in (
            'Запрос: ' + snapshot['request_id'], 'Задача: ' + snapshot['task_id'],
            'Исполнение: ' + snapshot['execution_id'], 'Вызов: ' + snapshot['tool_call_id'],
            'Сервер: ' + snapshot['server']['peer_name'], 'Команда: ' + snapshot['server']['command'],
            'Сервер arg[0] (10 байт): --readonly', 'Инструмент: ' + snapshot['tool_name'],
            'Аргументы (1800 байт, исходная строка):' + snapshot['arguments'],
        ):
            assert field in visible, field

    try:
        expect('KAIC CLI')
        send('/use ' + identity(1), 'Задача ' + identity(1))
        send('/tool', 'Следующая страница: /more')
        send('/approve ' + identity(101), 'Решение не подготовлено')
        assert not decisions
        send('/close', '')
        review()
        send('/use ' + identity(2), 'Задача ' + identity(2))
        send('/approve ' + identity(101), 'Отправить: /confirm ' + identity(101))
        send('/confirm ' + identity(102), 'UUID не совпадает')
        send('/confirm ' + identity(101), 'Команда принята backend')
        send('/confirm ' + identity(101), 'UUID не совпадает')
        assert decisions == [(identity(1), dict(request_id=identity(101), approve=True))]

        send('/tool', 'Следующая страница: /more')
        send('/deny ' + identity(102), 'Отправить: /confirm ' + identity(102))
        send('/confirm ' + identity(102), 'Команда принята backend')
        assert decisions[-1] == (identity(2), dict(request_id=identity(102), approve=False))

        with lock:
            tasks[identity(1)] = waiting(1, 103)
        send('/use ' + identity(1), 'Задача ' + identity(1))
        review()
        with lock:
            tasks[identity(1)]['pending_tool_approval']['arguments'] = 'изменено после просмотра'
        send('/history', 'Задача ' + identity(1))
        send('/approve ' + identity(103), 'Решение не подготовлено')
        send('/confirm ' + identity(103), 'Нет подготовленного')
        send('/close', '')
        send('/tool', 'страница 1/')
        send('/cancel', 'Команда принята backend')
        send('/deny ' + identity(103), 'Решение не подготовлено')
        send('/confirm ' + identity(103), 'Нет подготовленного')
        assert len(decisions) == 2
        assert writes == ['/tasks/' + identity(1) + '/tool-decision',
                          '/tasks/' + identity(2) + '/tool-decision',
                          '/tasks/' + identity(1) + '/cancel']

        # Неизвестный исход: просмотр, защита от устаревшего снимка и однократный ack.
        with lock:
            recovered = waiting(3, 104)
            snapshot = recovered.pop('pending_tool_approval')
            snapshot['arguments'] = '{"path":"result.txt"}'
            recovered['pending_tool_approval'] = None
            recovered['status'] = 'paused'
            recovered['recovery'] = dict(kind='unknown_outcome', message='Проверьте внешний результат', unresolved_tools=[snapshot])
            tasks[identity(3)] = recovered
        send('/use ' + identity(3), 'Задача ' + identity(3))
        send('/ack ' + identity(104), 'Сначала /recovery')
        send('/recovery', '/ack UUID фиксирует ознакомление')
        with lock:
            tasks[identity(3)]['recovery']['unresolved_tools'][0]['arguments'] = 'изменено'
        send('/ack ' + identity(104), 'Состояние изменилось')
        assert acknowledgments == []
        send('/recovery', '/ack UUID фиксирует ознакомление')
        send('/ack ' + identity(104), 'Ознакомление сохранено')
        send('/ack ' + identity(104), 'Сначала /recovery')
        assert acknowledgments == [(identity(3), dict(request_id=identity(104), acknowledge=True))]
        assert decisions == [(identity(1), dict(request_id=identity(101), approve=True)),
                             (identity(2), dict(request_id=identity(102), approve=False))]
        assert len(writes) == 4
        os.write(master, b'/quit\n')
        assert process.wait(timeout=5) == 0
        assert termios.tcgetattr(slave) == original

        # Отдельный процесс: SIGINT завершает CLI, не отменяя задачу backend.
        process = subprocess.Popen(
            [str(Path(binary).resolve())], stdin=slave, stdout=slave, stderr=slave,
            env={**os.environ, 'KAIC_CONTROL_TOKEN': 'pty-fixture-token-123456',
                 'KAIC_API_BASE': f'http://127.0.0.1:{server.server_port}'}, start_new_session=True)
        buffer.clear()
        expect('KAIC CLI')
        process.send_signal(signal.SIGINT)
        assert process.wait(timeout=5) == -signal.SIGINT
        assert termios.tcgetattr(slave) == original
        assert len(writes) == 4
        print('PTY: просмотр целиком, exact A после выбора B, wrong UUID, duplicate, deny, stale, cancel, recovery ack, quit, SIGINT — OK')
    finally:
        if process.poll() is None:
            process.kill()
            process.wait(timeout=5)
        os.close(master)
        os.close(slave)
        server.shutdown()


if __name__ == '__main__':
    if len(sys.argv) != 2:
        raise SystemExit('Запуск: python3 tests/cli_pty.py /путь/к/kaic-cli')
    main(sys.argv[1])
