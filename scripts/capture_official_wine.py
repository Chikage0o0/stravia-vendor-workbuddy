#!/usr/bin/env python3
"""Capture the packaged Windows WorkBuddy CLI against an isolated synthetic service.

Requires Linux user/mount/network namespaces, Wine, ip, mount and openssl. This
runs the real packaged CLI, not the desktop UI; no real accounts or upstreams.
"""
import argparse
import errno
import hashlib
import http.client
import json
import os
from pathlib import Path
import pty
import re
import signal
import socket
import ssl
import struct
import subprocess
import sys
import tempfile
import threading
import time
import zlib

from capture_workbuddy import installed_version

class Cursor:
    # Same bounded TLS vectors as the host capture-client hello parser.
    def __init__(self, data):
        self.data, self.pos = data, 0

    def take(self, length):
        if length < 0 or self.pos + length > len(self.data):
            raise ValueError('truncated TLS vector')
        data = self.data[self.pos:self.pos + length]
        self.pos += length
        return data

    def number(self, length):
        return int.from_bytes(self.take(length), 'big')

    def vector(self, length):
        return self.take(self.number(length))

    def done(self):
        return self.pos == len(self.data)


def tls_words(data):
    if len(data) % 2:
        raise ValueError('invalid TLS word vector')
    return [int.from_bytes(data[i:i + 2], 'big') for i in range(0, len(data), 2)]


def client_hello(conn):
    # Peek complete TLS records so SSL receives the original bytes unchanged.
    offset, handshake = 0, bytearray()
    for _ in range(256):
        header = conn.recv(offset + 5, socket.MSG_PEEK | socket.MSG_WAITALL)
        if len(header) < offset + 5 or header[offset] != 22:
            raise ValueError('missing TLS handshake record')
        length = int.from_bytes(header[offset + 3:offset + 5], 'big')
        if length > 18432 or offset + 5 + length > 262144:
            raise ValueError('TLS record limit')
        record = conn.recv(offset + 5 + length, socket.MSG_PEEK | socket.MSG_WAITALL)
        if len(record) < offset + 5 + length:
            raise ValueError('truncated TLS record')
        handshake.extend(record[offset + 5:offset + 5 + length])
        offset += 5 + length
        if len(handshake) >= 4:
            size = int.from_bytes(handshake[1:4], 'big')
            if handshake[0] != 1 or size + 4 > 262144:
                raise ValueError('invalid ClientHello')
            if len(handshake) >= size + 4:
                break
    else:
        raise ValueError('too many TLS records')
    c = Cursor(handshake[4:4 + size])
    legacy = c.number(2)
    c.take(32)  # random is secret-bearing dynamic data
    c.vector(1)  # session ID is never persisted
    suites = tls_words(c.vector(2))
    c.vector(1)
    result = {'legacy_version': legacy, 'cipher_suites': suites, 'extensions': [], 'extension_details': [],
              'supported_groups': [], 'ec_point_formats': [], 'signature_algorithms': [],
              'signature_algorithms_cert': [], 'certificate_compression': [], 'supported_versions': [],
              'key_share_groups': [], 'key_shares': [], 'padding_length': None, 'sni_present': False, 'alpn': []}
    ext = Cursor(c.vector(2) if not c.done() else b'')
    if not c.done():
        raise ValueError('invalid TLS extensions length')
    first_alpn, alpn_seen = None, False
    while not ext.done():
        kind, payload = ext.number(2), ext.vector(2)
        result['extensions'].append(kind)
        p = Cursor(payload)
        decoded = None
        if kind == 0:
            names = Cursor(p.vector(2))
            while not names.done():
                names.number(1)
                names.vector(2)  # hostnames never persisted
            result['sni_present'] = True
            decoded = {'server_name_present': True}
        elif kind in (10, 11, 13, 27, 43, 50):
            vector = p.vector(1 if kind in (11, 27, 43) else 2)
            values = list(vector) if kind == 11 else tls_words(vector)
            key, label = {10: ('supported_groups', 'groups'), 11: ('ec_point_formats', 'point_formats'),
                          13: ('signature_algorithms', 'algorithms'), 27: ('certificate_compression', 'algorithms'),
                          43: ('supported_versions', 'versions'), 50: ('signature_algorithms_cert', 'algorithms')}[kind]
            result[key] = values
            decoded = {label: values}
        elif kind == 16:
            inner = Cursor(p.vector(2))
            safe = (b'h2', b'http/1.1', b'http/1.0', b'h3', b'h3-29', b'acme-tls/1')
            while not inner.done():
                name = inner.vector(1)
                if not name:
                    raise ValueError('empty ALPN identifier')
                public_name = name.decode('ascii') if name in safe else None
                if not alpn_seen:
                    first_alpn, alpn_seen = public_name, True
                if public_name is not None:
                    result['alpn'].append(public_name)
            decoded = {'protocols': result['alpn']}
        elif kind == 51:
            shares = Cursor(p.vector(2))
            groups, sizes = [], []
            while not shares.done():
                group = shares.number(2)
                key_bytes = shares.vector(2)  # never persist key material
                groups.append(group)
                sizes.append({'group': group, 'length': len(key_bytes)})
            result['key_share_groups'], result['key_shares'] = groups, sizes
            decoded = {'groups': groups, 'shares': sizes}
        elif kind == 21:
            result['padding_length'] = len(payload)
            decoded = {'padding_length': len(payload)}
            p.take(len(payload))
        elif kind == 45:
            modes = p.vector(1)
            if all(mode <= 1 for mode in modes):
                decoded = {'psk_key_exchange_modes': list(modes)}
        else:
            # Unknown bodies, SNI, PSK identities and key material are never retained.
            p.take(len(payload))
            marker = {5: 'status_request', 18: 'signed_certificate_timestamp', 22: 'encrypt_then_mac', 23: 'extended_master_secret', 65281: 'renegotiation_info'}.get(kind)
            if marker:
                decoded = {marker: True}
        if not p.done():
            raise ValueError('invalid TLS extension')
        result['extension_details'].append({'type': kind, 'length': len(payload), 'decoded': decoded})
    def grease(number):
        return number & 0x0f0f == 0x0a0a and number >> 8 == number & 0xff
    def decimal(values):
        return '-'.join(str(n) for n in values)
    def hex_words(values):
        return ','.join(f'{n:04x}' for n in values)
    def hash12(text):
        return hashlib.sha256(text.encode()).hexdigest()[:12] if text else '000000000000'
    ciphers = [n for n in suites if not grease(n)]
    extensions = [n for n in result['extensions'] if not grease(n)]
    groups = [n for n in result['supported_groups'] if not grease(n)]
    signatures = [n for n in result['signature_algorithms'] if not grease(n)]
    ja3 = ','.join((str(legacy), decimal(ciphers), decimal(extensions), decimal(groups), decimal(result['ec_point_formats'])))
    versions = [n for n in result['supported_versions'] if not grease(n)]
    version = max(versions, default=legacy)
    version_code = {0x0304: '13', 0x0303: '12', 0x0302: '11', 0x0301: '10', 0x0300: 's3'}.get(version, '00')
    alpn_code = first_alpn[0] + first_alpn[-1] if first_alpn else '00'
    extension_input = hex_words(sorted(n for n in extensions if n not in (0, 16)))
    if signatures:
        extension_input += '_' + hex_words(signatures)
    result['ja3'] = ja3
    result['ja3_hash'] = hashlib.md5(ja3.encode(), usedforsecurity=False).hexdigest()
    result['ja4'] = (f"t{version_code}{'d' if result['sni_present'] else 'i'}"
                     f"{min(len(ciphers), 99):02}{min(len(extensions), 99):02}{alpn_code}_"
                     f"{hash12(hex_words(sorted(ciphers)))}_{hash12(extension_input)}")
    return result


HOSTS = {'cn': 'www.workbuddy.cn', 'intl': 'www.workbuddy.ai'}
PROMPT = 'Reply with the single word CAPTURE_SYNTHETIC_OK.'
SCHEMA_KEYS = frozenset('model messages role content stream temperature top_p max_tokens tools tool_choice name arguments function type tool_calls tool_call_id system user assistant'.split())
HEADER_NAMES = frozenset('x-codebuddy-request x-agent-runtime x-session-id x-parent-conversation-id x-compaction x-context-compacted x-data-tag x-model-id x-product-version x-api-key accept content-type x-requested-with x-stainless-arch x-stainless-lang x-stainless-os x-stainless-package-version x-stainless-retry-count x-stainless-runtime x-stainless-runtime-version x-stainless-timeout x-conversation-id x-agent-purpose x-ide-type x-ide-name x-ide-version x-agent-intent x-private-data x-request-id x-conversation-message-id x-conversation-request-id x-root-request-id x-agent-type acp-connection-id baggage content-encoding traceparent tracestate b3 x-b3-traceid x-b3-parentspanid x-b3-spanid x-b3-sampled x-trace-id authorization x-user-id x-product x-userinfo x-domain x-enterprise-id x-tenant-id user-agent accept-encoding accept-language connection host content-length transfer-encoding sec-fetch-mode'.split())
STATIC_HEADER_VALUES = {
    'accept': {'*/*', 'application/json', 'text/event-stream'},
    'content-type': {'application/json', 'application/json; charset=utf-8', 'application/json;charset=UTF-8', 'text/plain', 'text/plain;charset=UTF-8'},
    'content-encoding': {'gzip', 'identity'},
    'accept-encoding': {'gzip, deflate', 'br, gzip, deflate', 'gzip, deflate, br', 'identity',
                        'gzip, compress, deflate', 'gzip, compress, deflate, br',
                        'gzip, compress, deflate, zstd', 'gzip, compress, deflate, br, zstd'},
    'accept-language': {'*', 'zh-CN', 'zh-CN,zh;q=0.9', 'en-US', 'en-US,en;q=0.9'},
    'connection': {'keep-alive', 'close'}, 'sec-fetch-mode': {'cors'},
    'x-requested-with': {'XMLHttpRequest'}, 'x-stainless-lang': {'js'},
    'x-stainless-os': {'Windows'}, 'x-stainless-arch': {'x64'},
    'x-stainless-runtime': {'node'}, 'x-stainless-retry-count': {'0'},
    'x-stainless-timeout': {'600'}, 'x-ide-name': {'WorkBuddy'},
    'x-ide-type': {'WorkBuddy'}, 'x-product': {'WorkBuddy', 'workbuddy'},
    'user-agent': {'undici', 'node'},
    'x-agent-type': {'main', 'sub-agent', 'compaction'},
    'x-agent-runtime': {'local-agent', 'cloud-agent', 'agent-agent'},
    'x-agent-purpose': {'conversation', 'person_agent'},
    'x-agent-intent': {'craft'},
}
VERSION_HEADERS = {'x-stainless-package-version', 'x-stainless-runtime-version', 'x-ide-version'}


def public_header_value(name, value, public_values):
    if value in public_values.get(name, set()):
        return value
    if value in STATIC_HEADER_VALUES.get(name, set()):
        return value
    if name in VERSION_HEADERS and re.fullmatch(r'v?\d{1,3}(?:\.\d{1,5}){1,3}', value):
        return value
    if name == 'user-agent' and re.fullmatch(r'WorkBuddy/\d{1,3}(?:\.\d{1,5}){1,3} WorkBuddy/\d{1,3}(?:\.\d{1,5}){1,3} CLI/\d{1,3}(?:\.\d{1,5}){1,3}', value):
        return value
    return '<redacted>'


def decode_body(body, encoding):
    if encoding in ('', 'identity'):
        return body
    if encoding != 'gzip':
        raise ValueError('unsupported content encoding')
    decompressor = zlib.decompressobj(16 + zlib.MAX_WBITS)
    decoded = decompressor.decompress(body, 8 * 1024 * 1024 + 1)
    if len(decoded) > 8 * 1024 * 1024 or decompressor.unconsumed_tail or not decompressor.eof or decompressor.unused_data:
        raise ValueError('invalid or oversized gzip body')
    return decoded


def schema(value, depth=0):
    if depth >= 12:
        return {'type': 'depth_limit'}
    if isinstance(value, dict):
        return {'type': 'object', 'field_count': len(value), 'fields': [
            {'name': k if k in SCHEMA_KEYS else '<other>', 'schema': schema(v, depth + 1)}
            for k, v in list(value.items())[:256]]}
    if isinstance(value, list):
        return {'type': 'array', 'count': len(value), 'items': [schema(v, depth + 1) for v in value[:16]]}
    if value is None:
        return {'type': 'null'}
    if isinstance(value, bool):
        return {'type': 'boolean'}
    if isinstance(value, (int, float)):
        return {'type': 'number'}
    return {'type': 'string'}


def has_prompt(value):
    if isinstance(value, dict):
        return any(has_prompt(v) for v in value.values())
    if isinstance(value, list):
        return any(has_prompt(v) for v in value)
    return isinstance(value, str) and PROMPT in value


def serve_connection(conn, context, report, lock, connection_order, request_counter, public_values):
    stage = 'client_hello'
    try:
        conn.settimeout(15)
        hello = client_hello(conn)
        stage = 'tls_handshake'
        with context.wrap_socket(conn, server_side=True) as stream:
            stage = 'http_request'
            data = b''
            while b'\r\n\r\n' not in data:
                part = stream.recv(65536)
                if not part or len(data) + len(part) > 131072:
                    raise ValueError('invalid HTTP headers')
                data += part
            header, body = data.split(b'\r\n\r\n', 1)
            with lock:
                request_counter[0] += 1
                request_order = request_counter[0]
            lines = header.split(b'\r\n')
            parts = lines[0].decode('ascii').split(' ')
            if len(parts) != 3:
                raise ValueError('invalid request line')
            fields, length, content_encoding = [], 0, ''
            for line in lines[1:]:
                name, sep, value = line.partition(b':')
                if not sep or not re.fullmatch(rb"[!#$%&'*+.^_`|~0-9A-Za-z-]+", name):
                    raise ValueError('invalid header')
                spelling = name.decode('ascii')
                lower = spelling.lower()
                text = value.strip().decode('latin1')
                fields.append({'name': spelling if lower in HEADER_NAMES else '<other>', 'value': public_header_value(lower, text, public_values)})
                if lower == 'content-encoding':
                    if content_encoding:
                        raise ValueError('duplicate content encoding')
                    content_encoding = text.lower()
                if lower == 'content-length':
                    length = int(text)
                if lower == 'transfer-encoding':
                    raise ValueError('unsupported transfer encoding')
            if length < 0 or length > 8 * 1024 * 1024:
                raise ValueError('HTTP body limit')
            while len(body) < length:
                chunk = stream.recv(min(65536, length - len(body)))
                if not chunk:
                    raise ValueError('truncated body')
                body += chunk
            body = body[:length]
            decoded = decode_body(body, content_encoding)
            try:
                parsed = json.loads(decoded)
                shape = {'encoding': 'gzip-json' if content_encoding == 'gzip' else 'json', 'schema': schema(parsed)}
            except (ValueError, UnicodeDecodeError):
                parsed, shape = None, {'encoding': 'empty' if not decoded else 'opaque'}
            infer = parts[0] == 'POST' and parts[1].split('?', 1)[0] == '/v2/chat/completions'
            main = infer and has_prompt(parsed)
            tools = parsed.get('tools') if isinstance(parsed, dict) else None
            tools_count = len(tools) if isinstance(tools, list) else None
            with lock:
                report['requests'].append({'connection_order': connection_order, 'request_order': request_order, 'operation': 'infer' if infer else 'other', 'main_prompt_observed': main,
                    'tools_count': tools_count, 'client_hello': hello, 'headers': fields, 'body_bytes': len(body), 'body_schema': shape})
            if infer:
                payload = b'data: {"id":"synthetic","object":"chat.completion.chunk","choices":[{"index":0,"delta":{"content":"CAPTURE_SYNTHETIC_OK"},"finish_reason":null}]}\n\ndata: {"id":"synthetic","object":"chat.completion.chunk","choices":[{"index":0,"delta":{},"finish_reason":"stop"}]}\n\ndata: [DONE]\n\n'
                status, ctype = '200 OK', 'text/event-stream'
            else:
                payload, status, ctype = b'{"error":"synthetic local endpoint"}', '404 Not Found', 'application/json'
            stream.sendall(f'HTTP/1.1 {status}\r\nContent-Type: {ctype}\r\nContent-Length: {len(payload)}\r\nConnection: close\r\n\r\n'.encode() + payload)
    except Exception as error:
        with lock:
            report['failures'].append({'stage': stage, 'kind': type(error).__name__})
    finally:
        conn.close()


class LocalHTTPError(Exception):
    def __init__(self, status):
        self.status = status
        super().__init__('local service returned non-200')


def local_request(port, path, body=None, headers=None):
    connection = http.client.HTTPConnection('127.0.0.1', port, timeout=20)
    try:
        encoded = json.dumps(body).encode() if body is not None else None
        hdrs = {'Content-Type': 'application/json', 'Accept': 'application/json, text/event-stream', 'X-CodeBuddy-Request': '1'}
        hdrs.update(headers or {})
        connection.request('POST' if body is not None else 'GET', path, encoded, hdrs)
        response = connection.getresponse()
        if response.status != 200:
            raise LocalHTTPError(response.status)
        if 'text/event-stream' in response.getheader('content-type', ''):
            wanted = body.get('id') if body else None
            while True:
                line = response.readline(1024 * 1024)
                if not line:
                    raise RuntimeError('SSE ended before response')
                if line.startswith(b'data:'):
                    item = json.loads(line[5:].strip())
                    if item.get('id') == wanted:
                        return item
        return json.loads(response.read(4 * 1024 * 1024))
    finally:
        connection.close()


def result(response):
    if 'error' in response or not isinstance(response.get('result'), dict):
        raise RuntimeError('ACP request failed')
    return response['result']


STARTUP_DIAGNOSTICS = {
    'module_not_found': (b'err_module_not_found', b'cannot find module', b'cannot find package', b'module_not_found'),
    'entry_not_found': (b'enoent', b'no such file or directory', b'file not found', b'path not found'),
    'wine_loader_error': (b'wine: could not load', b'wine: failed to open', b'wine: could not open', b'wine: could not find'),
    'missing_dll': (b'library not found', b'dll not found', b'could not be found', b'module not found'),
    'invalid_executable': (b'bad exe format', b'invalid executable', b'invalid image format'),
    'syntax_error': (b'syntaxerror:',),
    'permission_denied': (b'eacces', b'permission denied', b'access denied'),
    'address_in_use': (b'eaddrinuse', b'address already in use'),
    'certificate_error': (b'certificate_verify_failed', b'unable to verify the first certificate', b'unable to get local issuer certificate'),
    'node_fatal_error': (b'fatal error:', b'javascript heap out of memory'),
    'uncaught_exception': (b'uncaughtexception', b'uncaught exception', b'triggeruncaughtexception', b'uncaught typeerror', b'uncaught error'),
    'type_error': (b'typeerror:',),
    'reference_error': (b'referenceerror:',),
    'range_error': (b'rangeerror:',),
    'node_option_error': (b'bad option:', b'unknown or unexpected option', b'unknown option', b'unknown argument'),
    'cwd_error': (b'uv_cwd', b'process.cwd()', b'getcwd', b'current directory'),
    'wine_initialization_error': (b'wine: configuration', b'wine: created the configuration', b'wineboot', b'wineserver'),
    'node_electron_conflict': (b'electron_run_as_node', b'run_as_node'),
    'native_module_error': (b'err_dlopen_failed', b'module did not self-register', b'cannot open shared object file', b'wrong elf class'),
    'generic_error': (b'error:', b'exception:')
}
# These finite vocabularies identify known runtime failures without echoing
# arbitrary module names, paths, messages, request data or credentials.
DIAGNOSTIC_TOKENS = {
    'error_codes': ('ERR_MODULE_NOT_FOUND', 'MODULE_NOT_FOUND', 'ERR_REQUIRE_ESM', 'ERR_UNKNOWN_FILE_EXTENSION',
        'ERR_INVALID_MODULE_SPECIFIER', 'ERR_PACKAGE_PATH_NOT_EXPORTED', 'ERR_DLOPEN_FAILED', 'ERR_INVALID_ARG_TYPE',
        'ERR_INVALID_ARG_VALUE', 'ERR_UNKNOWN_OPTION', 'ERR_PARSE_ARGS_UNKNOWN_OPTION', 'ERR_WORKER_INIT_FAILED',
        'ENOENT', 'EACCES', 'EPERM', 'EADDRINUSE', 'ECONNREFUSED', 'ENOTDIR', 'EISDIR', 'ENOSPC', 'ENOMEM',
        'CERT_HAS_EXPIRED', 'DEPTH_ZERO_SELF_SIGNED_CERT', 'UNABLE_TO_VERIFY_LEAF_SIGNATURE',
        'UNABLE_TO_GET_ISSUER_CERT_LOCALLY', 'SELF_SIGNED_CERT_IN_CHAIN'),
    'known_modules': ('electron', 'codebuddy-lite-wb.mjs', 'product.json', 'app.asar', 'node:fs', 'node:path',
        'node:module', 'node:internal/modules/esm/resolve', 'node:internal/modules/cjs/loader'),
    'known_libraries': ('ntdll.dll', 'kernel32.dll', 'kernelbase.dll', 'user32.dll', 'advapi32.dll', 'ole32.dll',
        'msvcrt.dll', 'ucrtbase.dll', 'vcruntime140.dll', 'msvcp140.dll', 'api-ms-win-crt-runtime-l1-1-0.dll',
        'libwine.so', 'libc.so.6', 'libstdc++.so.6', 'libgcc_s.so.1'),
    'known_status': ('c0000135', 'c000007b', 'c0000142', 'c0000005', 'c0000022', '80070002', '80070005')
}


def scrub_diagnostic_line(line):
    # Opt-in log is still sensitive operational data. Reject entire lines that
    # might carry authentication, session material, or serialized requests.
    text = line.decode('utf-8', errors='replace')
    if re.search(r'(?i)token|session|authoriz|cookie|secret|credential|password|bearer|api[-_]?key|synthetic[-_]|\{|\}', text):
        return '[sensitive line omitted]\n'
    text = re.sub(r'''["'][^"'\r\n]*["']''', '<quoted>', text)
    text = re.sub(r'(?i)\b(?:https?|file)://[^\s]+', '<url>', text)
    text = re.sub(r'(?i)\b[a-z]:[\\/].*', '<path>', text)
    text = re.sub(r'(?<![\w])/(?:[^\s:/]+/)*[^\s:]*.*', '<path>', text)
    text = re.sub(r'\b[A-Za-z0-9_+/=-]{40,}\b', '<opaque>', text)
    return text.rstrip('\r\n') + '\n'


def classify_output(stream, diagnostics, private_log, output_lock):
    # Drain continuously without retaining or displaying raw client output. Keep
    # only a short transient overlap so fixed markers split across reads match.
    overlap = b''
    pending = bytearray()
    dropping_line = False
    def log_line(line):
        if private_log is None:
            return
        safe = scrub_diagnostic_line(line).encode('utf-8')
        with output_lock:
            remaining = 256 * 1024 - diagnostics['log_bytes']
            if remaining > 0:
                count = os.write(private_log, safe[:remaining])
                diagnostics['log_bytes'] += count
    try:
        while True:
            try:
                chunk = stream.read1(65536)
            except OSError as error:
                if error.errno != errno.EIO:
                    raise
                chunk = b''
            if not chunk:
                if pending and not dropping_line:
                    log_line(pending)
                break
            if private_log is not None:
                for part in chunk.splitlines(keepends=True):
                    complete = part.endswith(b'\n') or part.endswith(b'\r')
                    if not dropping_line:
                        if len(pending) + len(part) > 8192:
                            pending.clear()
                            dropping_line = True
                            log_line(b'[oversized line omitted]')
                        else:
                            pending.extend(part)
                    if complete:
                        if not dropping_line:
                            log_line(pending)
                        pending.clear()
                        dropping_line = False
            text = (overlap + chunk).lower()
            with output_lock:
                diagnostics['output_seen'] = True
                for category, markers in STARTUP_DIAGNOSTICS.items():
                    if any(marker in text for marker in markers):
                        diagnostics['categories'].add(category)
                for field, tokens in DIAGNOSTIC_TOKENS.items():
                    for token in tokens:
                        if token.lower().encode('ascii') in text:
                            diagnostics[field].add(token)
            overlap = text[-128:]
    finally:
        stream.close()


def desktop_product_name(application):
    # main/index.js sets assets root to app.asar/main. Resolve the same
    # packaged candidates as resolveBundledAsset('product.json'), in order.
    resources = application / 'resources'
    unpacked = resources / 'app.asar.unpacked'
    def product_name(data):
        try:
            name = json.loads(data).get('productName')
            return name if isinstance(name, str) and name else 'WorkBuddy'
        except (ValueError, TypeError, AttributeError):
            return 'WorkBuddy'
    for path in (unpacked / 'resources' / 'product.json', unpacked / 'product.json'):
        if path.exists():
            try:
                return product_name(path.read_bytes()), 'desktop_unpacked_resource'
            except OSError:
                return 'WorkBuddy', 'desktop_fallback'
    with (resources / 'app.asar').open('rb') as stream:
        prefix = stream.read(16)
        if len(prefix) != 16:
            raise ValueError('truncated ASAR header')
        marker, header_size, _, json_size = struct.unpack('<4I', prefix)
        if marker != 4 or header_size < 8 or json_size > header_size - 8 or header_size > 64 * 1024 * 1024:
            raise ValueError('invalid ASAR header')
        header = json.loads(stream.read(json_size))
        for parts in (('main', 'product.json'), ('resources', 'product.json'), ('main', 'resources', 'product.json')):
            entry = header
            try:
                for part in parts:
                    entry = entry['files'][part]
            except KeyError:
                continue
            try:
                size = entry['size']
                if not isinstance(size, int) or not 0 <= size <= 1024 * 1024:
                    raise ValueError('invalid product entry')
                if entry.get('unpacked'):
                    data = unpacked.joinpath(*parts).read_bytes()
                else:
                    offset = int(entry['offset'])
                    if offset < 0:
                        raise ValueError('invalid product offset')
                    stream.seek(8 + header_size + offset)
                    data = stream.read(size)
                return product_name(data), 'desktop_asar_resource'
            except (OSError, KeyError, TypeError, ValueError):
                return 'WorkBuddy', 'desktop_fallback'
    return 'WorkBuddy', 'desktop_fallback'


def inside(args):
    for name in ('user', 'mnt', 'net'):
        original = int(os.environ['WORKBUDDY_CAPTURE_NS_' + name.upper()])
        if os.stat('/proc/self/ns/' + name).st_ino == original:
            raise RuntimeError('isolation unavailable')
    root = Path(os.environ['WORKBUDDY_CAPTURE_ROOT'])
    host = HOSTS[args.region]
    home = root / 'home'
    home.mkdir(mode=0o700)
    (root / 'hosts').write_text('127.0.0.1 localhost ' + host + '\n::1 localhost\n')
    (root / 'nsswitch').write_text('hosts: files dns\n')
    (root / 'resolv').write_text('nameserver 127.0.0.1\n')
    for name, dest in [('hosts', '/etc/hosts'), ('nsswitch', '/etc/nsswitch.conf'), ('resolv', '/etc/resolv.conf')]:
        subprocess.run(['mount', '--bind', str(root / name), dest], check=True)
    for dest in ('/run/systemd/resolve/io.systemd.Resolve', '/var/run/nscd/socket'):
        if os.path.exists(dest):
            subprocess.run(['mount', '--bind', '/dev/null', dest], check=True)
    subprocess.run(['ip', 'link', 'set', 'lo', 'up'], check=True)
    ca, key = root / 'ca.pem', root / 'key.pem'
    subprocess.run(['openssl', 'req', '-x509', '-newkey', 'rsa:2048', '-nodes', '-keyout', str(key), '-out', str(ca), '-days', '1', '-subj', '/CN=synthetic WorkBuddy capture', '-addext', 'subjectAltName=DNS:' + host, '-addext', 'basicConstraints=critical,CA:TRUE'], check=True, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    if args.cold_capture:
        context.num_tickets = 0
        context.options |= ssl.OP_NO_TICKET
    context.load_cert_chain(ca, key)
    context.set_alpn_protocols(['http/1.1'])
    listener = socket.socket()
    listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    listener.bind(('127.0.0.1', 443))
    listener.listen(32)
    listener.settimeout(.5)
    cli_root = (args.application / args.entry).parent.parent
    cli_package = json.loads((cli_root / 'package.json').read_text(encoding='utf-8'))
    product = json.loads((cli_root / 'product.json').read_text(encoding='utf-8'))
    expected_identity = {'cn': ('WorkBuddy', 'WorkBuddy'), 'intl': ('WorkBuddy AI', 'workbuddy-ai')}[args.region]
    if product.get('endpoint') != 'https://' + host or (product.get('productName'), product.get('applicationName')) != expected_identity:
        raise ValueError('bundled product does not match selected region')
    defaults = [model for model in product.get('models', []) if isinstance(model, dict) and model.get('isDefault') is True]
    if len(defaults) != 1 or not isinstance(defaults[0].get('id'), str) or not defaults[0]['id'] or defaults[0].get('url'):
        raise ValueError('bundled default model is ambiguous or has a separate URL')
    model_id = defaults[0]['id']
    cli_version = cli_package.get('version')
    if not cli_version or cli_version == '0.0.0':
        cli_version = cli_package.get('publishConfig', {}).get('customPackage', {}).get('version')
    if not isinstance(cli_version, str) or not cli_version:
        raise ValueError('bundled CLI version unavailable')
    with (args.application / args.exe).open('rb') as exe_file, (args.application / args.entry).open('rb') as entry_file:
        provenance = {'product_version': installed_version(args.application), 'cli_version': cli_version, 'exe_sha256': hashlib.file_digest(exe_file, 'sha256').hexdigest(), 'entry_sha256': hashlib.file_digest(entry_file, 'sha256').hexdigest()}
    desktop_version = provenance['product_version']
    if not all(re.fullmatch(r'\d{1,3}(?:\.\d{1,5}){2,3}', version) for version in (desktop_version, cli_version)):
        raise ValueError('unrecognized bundled version')
    cli_product_name, application_name = expected_identity
    desktop_name, desktop_name_source = desktop_product_name(args.application)
    if desktop_name not in ('WorkBuddy', 'WorkBuddy AI'):
        raise ValueError('unrecognized desktop product name')
    provenance['desktop_product_name'] = desktop_name
    provenance['desktop_product_name_source'] = desktop_name_source
    public_values = {'host': {host}, 'user-agent': {f'WorkBuddy/{desktop_version} {application_name}/{desktop_version} CLI/{cli_version}'},
                     'x-product': {cli_product_name, application_name}}
    report = {'client': 'workbuddy-windows-packaged-cli', 'region': args.region, 'provenance': provenance, 'cold_capture': args.cold_capture, 'status': 'failed', 'boundary': 'synthetic local service; not desktop UI or upstream success', 'failures': [], 'requests': []}
    lock, stop, workers = threading.Lock(), threading.Event(), []
    request_counter = [0]
    def serve():
        connection_order = 0
        while not stop.is_set():
            try:
                conn, _ = listener.accept()
            except socket.timeout:
                continue
            except OSError:
                break
            connection_order += 1
            worker = threading.Thread(target=serve_connection, args=(conn, context, report, lock, connection_order, request_counter, public_values), daemon=True)
            workers.append(worker)
            worker.start()
    server = threading.Thread(target=serve, daemon=True)
    server.start()
    env = {'PATH': os.environ.get('PATH', '/usr/bin:/bin'), 'HOME': str(home), 'WINEPREFIX': str(root / 'wine'), 'WINEDEBUG': '-all', 'LANG': 'C.UTF-8'}
    process = None
    output_threads = []
    output_lock = threading.Lock()
    diagnostics = {'output_seen': False, 'log_bytes': 0, 'categories': set(), 'connect_http_statuses': set(), **{field: set() for field in DIAGNOSTIC_TOKENS}}
    stage = 'wine_setup'
    try:
        subprocess.run([args.wine, 'wineboot', '-u'], env=env, check=True, timeout=120, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        dos = root / 'wine' / 'dosdevices'
        (dos / 'z:').unlink(missing_ok=True)
        (dos / 'w:').symlink_to(args.application, target_is_directory=True)
        (dos / 's:').symlink_to(root, target_is_directory=True)
        env.update({'ELECTRON_RUN_AS_NODE': '1', 'NODE_EXTRA_CA_CERTS': r'S:\ca.pem', 'CODEBUDDY_AUTH_TOKEN': 'synthetic-token', 'ACC_USER_ID': 'synthetic-user', 'ACC_USER_NICKNAME': 'synthetic', 'CODEBUDDY_REMOTE_CONFIG_DISABLED': '1', 'CODEBUDDY_FORCE_LITE_WB_BUNDLE': '1', 'CODEBUDDY_FORCE_HEADLESS_BUNDLE': '1', 'CODEBUDDY_SKIP_GIT_BASH_CHECK': '1'})
        desktop_version = provenance['product_version']
        env.update({
            'CLIENT_INFO_PLATFORM': 'WorkBuddy', 'CLIENT_INFO_PLATFORM_VERSION': desktop_version,
            'CLIENT_INFO_IDE_TYPE': 'WorkBuddy', 'CLIENT_INFO_PRODUCT_NAME': desktop_name,
            'CLIENT_INFO_PRODUCT_VERSION': desktop_version, 'CLIENT_INFO_PLUGIN_NAME': 'workbuddy-desktop',
            'CLIENT_INFO_PLUGIN_VERSION': desktop_version, 'CLIENT_INFO_USER_AGENT_EXTENSION': 'CLI/' + cli_version,
            'CLIENT_INFO_MACHINE_ID': '00000000-0000-4000-8000-000000000001',
            'CODEBUDDY_HOST': 'workbuddy-desktop', 'CODEBUDDY_CONFIG_DIR': r'S:\home\config',
            'WORKBUDDY_CONFIG_DIR': r'S:\home\config', 'WORKBUDDY_DATA_FOLDER_NAME': '.workbuddy',
            'DISABLE_AUTOUPDATER': '1', 'CODEBUDDY_GIT_REPO_SCAN_DISABLED': '1',
            'CODEBUDDY_PROMPT_SUGGESTION_DISABLED': '1', 'CODEBUDDY_DISABLE_CRON': '1',
            'CODEBUDDY_DISABLE_PRODUCT_CACHE': '1', 'CODEBUDDY_INCLUDE_TOPIC_MESSAGE': '1',
            'CODEBUDDY_CODE_DISABLE_SESSION_SUMMARY': '1', 'CODEBUDDY_DISABLE_AUTO_MEMORY': '1',
            'CODEBUDDY_DISABLE_SYSTEM_REMINDER_MD': '1',
        })
        (home / 'config').mkdir(mode=0o700)
        exe = 'W:\\' + str(args.exe).replace('/', '\\')
        entry = 'W:\\' + str(args.entry).replace('/', '\\')
        stage = 'cli_start'
        master, slave = pty.openpty()
        try:
            process = subprocess.Popen([args.wine, exe, entry, '--serve', '--host', '127.0.0.1', '--port', str(args.port), '--model', model_id], cwd=home, env=env, stdin=slave, stdout=slave, stderr=slave, start_new_session=True)
        except BaseException:
            os.close(master)
            raise
        finally:
            os.close(slave)
        reader = threading.Thread(target=classify_output, args=(os.fdopen(master, 'rb'), diagnostics, args.diagnostic_fd, output_lock), daemon=True)
        output_threads.append(reader)
        reader.start()
        deadline = time.monotonic() + args.timeout
        while True:
            exit_code = process.poll()
            if exit_code is not None:
                diagnostics['early_exit_code'] = exit_code
                raise RuntimeError('CLI exited before connecting')
            try:
                connection = local_request(args.port, '/api/v1/acp/connect', {})
                diagnostics['connect_http_statuses'].add(200)
                break
            except LocalHTTPError as error:
                diagnostics['connect_http_statuses'].add(error.status)
                # A 404 is not proof of readiness or of transient startup: ACP
                # may not be active, or the wrong process may own this port.
                if error.status != 404:
                    raise
                if time.monotonic() >= deadline:
                    raise TimeoutError('ACP connect remained unavailable') from None
                time.sleep(.25)
            except (OSError, http.client.HTTPException):
                if time.monotonic() >= deadline:
                    raise TimeoutError('local service not ready')
                time.sleep(.25)
        if 'data' in connection:
            connection = connection['data']
        headers = {'acp-connection-id': connection['connectionId'], 'acp-session-token': connection['sessionToken']}
        def rpc(number, method, params):
            return result(local_request(args.port, '/api/v1/acp', {'jsonrpc': '2.0', 'id': number, 'method': method, 'params': params}, headers))
        stage = 'acp_initialize'
        rpc(1, 'initialize', {'protocolVersion': 1, 'clientInfo': {'name': 'capture', 'version': '1'}, 'clientCapabilities': {'fs': {'readTextFile': False, 'writeTextFile': False}}})
        stage = 'acp_session'
        session = rpc(2, 'session/new', {'mcpServers': [], 'cwd': r'S:\home', 'workingDirectory': r'S:\home'})
        models = session.get('models', {})
        if models.get('currentModelId') != model_id:
            raise RuntimeError('default model not confirmed')
        stage = 'acp_prompt'
        rpc(3, 'session/prompt', {'sessionId': session['sessionId'], 'prompt': [{'type': 'text', 'text': PROMPT}]})
    except Exception as error:
        failure = {'stage': stage, 'kind': type(error).__name__}
        if isinstance(error, LocalHTTPError):
            failure['http_status'] = error.status
        report['failures'].append(failure)
    finally:
        try:
            subprocess.run([str(args.wineserver), '-k'], env=env, check=True, timeout=10, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        except (OSError, subprocess.SubprocessError) as error:
            report['failures'].append({'stage': 'wineserver_shutdown', 'kind': type(error).__name__})
        if process is not None:
            try:
                os.killpg(process.pid, signal.SIGTERM)
            except ProcessLookupError:
                pass
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                os.killpg(process.pid, signal.SIGKILL)
                process.wait()
        if output_threads:
            for reader in output_threads:
                reader.join(timeout=3)
            if any(reader.is_alive() for reader in output_threads):
                report['failures'].append({'stage': 'output_shutdown', 'kind': 'worker_timeout'})
            with output_lock:
                report['startup_diagnostics'] = {'output_seen': diagnostics['output_seen'],
                    'categories': sorted(diagnostics['categories']),
                    'early_exit_code': diagnostics.get('early_exit_code'),
                    'connect_http_statuses': sorted(diagnostics['connect_http_statuses']),
                    **{field: sorted(diagnostics[field]) for field in DIAGNOSTIC_TOKENS}}
        stop.set()
        listener.close()
        server.join(timeout=2)
        for worker in workers:
            worker.join(timeout=16)
        if server.is_alive() or any(worker.is_alive() for worker in workers):
            report['failures'].append({'stage': 'shutdown', 'kind': 'worker_timeout'})
        report['requests'].sort(key=lambda request: request['request_order'])
        if not any(r['main_prompt_observed'] for r in report['requests']):
            report['failures'].append({'stage': 'infer', 'kind': 'main_prompt_not_observed'})
        if not report['failures']:
            report['status'] = 'captured'
        args.output.write_text(json.dumps(report, indent=2) + '\n')
    if report['failures']:
        print('Capture failed; see redacted report: ' + ', '.join(item['stage'] + '/' + item['kind'] for item in report['failures']), file=sys.stderr)
        raise SystemExit(1)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--region', choices=('cn', 'intl'), default='cn', help='official product region (default: cn)')
    parser.add_argument('--application', required=True, type=Path, help='extracted Windows application directory')
    parser.add_argument('--exe', required=True, type=Path, help='executable relative to application directory')
    parser.add_argument('--entry', required=True, type=Path, help='packaged CLI entry relative to application directory')
    parser.add_argument('--wine', required=True, type=Path, help='Wine executable')
    parser.add_argument('--wineserver', type=Path, help='wineserver executable; defaults to sibling of --wine')
    parser.add_argument('--output', required=True, type=Path, help='redacted JSON report')
    parser.add_argument('--diagnostic-log', type=Path, help='opt-in private filtered startup log, created exclusively with mode 0600; synthetic environment only')
    parser.add_argument('--diagnostic-fd', type=int, help=argparse.SUPPRESS)
    parser.add_argument('--temp-parent', type=Path, help='parent of temporary private Wine profile')
    parser.add_argument('--port', type=int, default=18765, help='local ACP service port')
    parser.add_argument('--cold-capture', action='store_true', help='disable TLS session tickets for fresh connection capture')
    parser.add_argument('--timeout', type=int, default=90, help='local service startup deadline in seconds')
    parser.add_argument('--inside', action='store_true', help=argparse.SUPPRESS)
    args = parser.parse_args()
    args.application, args.wine, args.output = args.application.resolve(), args.wine.resolve(), args.output.resolve()
    args.wineserver = (args.wineserver or args.wine.with_name('wineserver')).resolve()
    if not 1024 <= args.port <= 65535 or args.timeout <= 0:
        parser.error('invalid port or timeout')
    for name in ('exe', 'entry'):
        relative = getattr(args, name)
        if relative.is_absolute() or '..' in relative.parts or not (args.application / relative).is_file() or not (args.application / relative).resolve().is_relative_to(args.application):
            parser.error(name + ' must identify a file inside the application directory')
    if not args.inside and args.diagnostic_fd is not None:
        parser.error('--diagnostic-fd is internal')
    if args.inside and args.diagnostic_log is not None:
        parser.error('--diagnostic-log is handled by the outer process')
    os.umask(0o077)
    if args.inside:
        try:
            inside(args)
        except Exception as error:
            args.output.write_text(json.dumps({'client': 'workbuddy-windows-packaged-cli', 'status': 'failed', 'failures': [{'stage': 'isolation_or_setup', 'kind': type(error).__name__}], 'requests': []}, indent=2) + '\n')
            raise SystemExit(1) from None
        return
    args.output.write_text(json.dumps({'client': 'workbuddy-windows-packaged-cli', 'status': 'failed', 'failures': [{'stage': 'isolation', 'kind': 'capture_not_started'}], 'requests': []}, indent=2) + '\n')
    with tempfile.TemporaryDirectory(prefix='workbuddy-capture-', dir=args.temp_parent) as root:
        env = {'PATH': os.environ.get('PATH', '/usr/bin:/bin'), 'WORKBUDDY_CAPTURE_ROOT': root}
        for name in ('user', 'mnt', 'net'):
            env['WORKBUDDY_CAPTURE_NS_' + name.upper()] = str(os.stat('/proc/self/ns/' + name).st_ino)
        command = ['unshare', '--user', '--map-root-user', '--mount', '--net', '--fork', sys.executable, str(Path(__file__).resolve()), '--inside', '--application', str(args.application), '--exe', str(args.exe), '--entry', str(args.entry), '--wine', str(args.wine), '--wineserver', str(args.wineserver), '--output', str(args.output), '--port', str(args.port), '--timeout', str(args.timeout)]
        command.extend(['--region', args.region])
        if args.cold_capture:
            command.append('--cold-capture')
        diagnostic_fd = None
        try:
            if args.diagnostic_log is not None:
                diagnostic_fd = os.open(args.diagnostic_log, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW, 0o600)
                command.extend(['--diagnostic-fd', str(diagnostic_fd)])
            subprocess.run(command, env=env, pass_fds=(diagnostic_fd,) if diagnostic_fd is not None else (), check=True)
        finally:
            if diagnostic_fd is not None:
                os.close(diagnostic_fd)


if __name__ == '__main__':
    main()
