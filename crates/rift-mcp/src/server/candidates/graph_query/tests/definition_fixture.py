"""Recorded definition responses for package-target inventory tests."""
import json
import re
import sys

sources = {}


def send(identifier, result):
    payload = json.dumps({'jsonrpc': '2.0', 'id': identifier, 'result': result}).encode()
    sys.stdout.buffer.write(f'Content-Length: {len(payload)}\r\n\r\n'.encode() + payload)
    sys.stdout.buffer.flush()


while True:
    headers = {}
    while True:
        line = sys.stdin.buffer.readline()
        if not line:
            sys.exit(0)
        if line in (b'\r\n', b'\n'):
            break
        name, value = line.decode().split(':', 1)
        headers[name.lower()] = value.strip()
    message = json.loads(sys.stdin.buffer.read(int(headers['content-length'])))
    method = message.get('method')
    params = message.get('params', {})
    identifier = message.get('id')
    if method == 'exit':
        break
    if method == 'textDocument/didOpen':
        document = params['textDocument']
        sources[document['uri']] = document['text']
    if identifier is None:
        continue
    if method == 'initialize':
        result = {'capabilities': {'positionEncoding': 'utf-16', 'textDocumentSync': 1,
                                   'definitionProvider': True}}
    elif method == 'textDocument/definition':
        uri = params['textDocument']['uri']
        position = params['position']
        line = sources[uri].splitlines()[position['line']]
        word = re.search(r'[A-Za-z_][A-Za-z_0-9]*$', line[:position['character'] + 1])
        name = word.group() if word else ''
        if name == 'to_string':
            result = [{'uri': sys.argv[1], 'range': {'start': {'line': 0, 'character': 7},
                                                     'end': {'line': 0, 'character': 16}}}]
        elif name == 'unknown':
            result = [{'uri': sys.argv[2], 'range': {'start': {'line': 0, 'character': 7},
                                                     'end': {'line': 0, 'character': 14}}}]
        elif name == 'local':
            result = [{'uri': uri, 'range': {'start': {'line': 0, 'character': 7},
                                             'end': {'line': 0, 'character': 12}}}]
        else:
            result = []
    else:
        result = None
    send(identifier, result)
