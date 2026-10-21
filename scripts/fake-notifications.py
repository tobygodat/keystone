"""A fake org.freedesktop.Notifications server for native verification.

Usage: uv run --no-project --with jeepney python scripts/fake-notifications.py BUS LOG COMMANDS

Owns org.freedesktop.Notifications on BUS, the private bus the native driver's
`start` returns, and refuses the user's own session bus. Each Notify call is appended to LOG as one JSON line and
answered with a new id. Lines appended to COMMANDS ask it to emit signals:
`invoke ID ACTION` sends ActionInvoked, `close ID` sends NotificationClosed.
"""
import json
import os
import sys
import time

from jeepney import DBusAddress, MessageType, HeaderFields, new_method_return, new_signal
from jeepney.bus_messages import message_bus
from jeepney.io.blocking import open_dbus_connection

address, log_path, commands_path = sys.argv[1:4]
if address.startswith('unix:path=/run/user/') or address == os.environ.get('DBUS_SESSION_BUS_ADDRESS'):
    sys.exit('refusing to own notifications on the user session bus')
conn = open_dbus_connection(bus=address)
reply = conn.send_and_get_reply(message_bus.RequestName('org.freedesktop.Notifications', 4))
if reply.body[0] != 1:
    sys.exit(f'could not own the name: {reply.body}')
path = DBusAddress('/org/freedesktop/Notifications', interface='org.freedesktop.Notifications')
next_id = 0
offset = 0
open(commands_path, 'a').close()
print(json.dumps({'ready': True, 'unique_name': conn.unique_name}), flush=True)


def record(entry):
    with open(log_path, 'a') as log:
        log.write(json.dumps(entry, default=repr) + '\n')


while True:
    try:
        msg = conn.receive(timeout=0.05)
    except TimeoutError:
        msg = None
    if msg is not None and msg.header.message_type == MessageType.method_call:
        member = msg.header.fields.get(HeaderFields.member)
        if member == 'Notify':
            next_id += 1
            app, replaces, icon, summary, body, actions, hints, timeout = msg.body
            record({'time': time.time(), 'id': next_id, 'app_name': app, 'replaces_id': replaces,
                    'summary': summary, 'body': body, 'actions': list(actions),
                    'hints': sorted(hints), 'expire_timeout': timeout,
                    'sender': msg.header.fields.get(HeaderFields.sender)})
            conn.send(new_method_return(msg, 'u', (next_id,)))
        elif member == 'GetCapabilities':
            conn.send(new_method_return(msg, 'as', (['actions', 'body'],)))
        elif member == 'GetServerInformation':
            conn.send(new_method_return(msg, 'ssss', ('keystone-fake', 'keystone', '1', '1.2')))
        elif member == 'CloseNotification':
            conn.send(new_method_return(msg, '', ()))
        else:
            record({'time': time.time(), 'unhandled': member})
    with open(commands_path) as commands:
        commands.seek(offset)
        lines = commands.read()
    # Only whole lines: a command still being appended waits for its newline.
    lines = lines[:lines.rfind('\n') + 1]
    offset += len(lines)
    for line in lines.splitlines():
        parts = line.split()
        if parts[:1] == ['invoke']:
            conn.send(new_signal(path, 'ActionInvoked', 'us', (int(parts[1]), parts[2])))
            record({'time': time.time(), 'emitted': 'ActionInvoked', 'id': int(parts[1]), 'action': parts[2]})
        elif parts[:1] == ['close']:
            conn.send(new_signal(path, 'NotificationClosed', 'uu', (int(parts[1]), 2)))
            record({'time': time.time(), 'emitted': 'NotificationClosed', 'id': int(parts[1])})
