# Third-party notices

## zca-js

Parts of this crate are Rust translations of [zca-js](https://github.com/RFS-ADRENO/zca-js)
(version 2.2.0), the unofficial Zalo API library for JavaScript, which is distributed under the
MIT License below. Its copyright holders are RFS-ADRENO, truong9c2208 and JustKemForFun.

Modules that are function-by-function ports of zca-js code:

| Module | Ported from |
| --- | --- |
| `src/crypto.rs` | `utils` (AES/MD5 helpers, request signing) |
| `src/params.rs` | `ParamsEncryptor` |
| `src/events.rs` | `apis/listen` framing, `decodeEventData` |
| `src/session.rs` | `apis/login`, `Zalo.loginCookie` |
| `src/login_qr.rs` | `apis/loginQR` |
| `src/ws.rs` | `Listener` |

Modules that follow zca-js's request shapes, field names and rules (`sendMessage`, `uploadAttachment`, `addReaction`,
`getStickers`, `UserMessage` / `GroupMessage`, `Undo`, ...) and are derived from them:
`src/api.rs`, `src/message.rs`, `src/react.rs`, `src/sticker.rs`, `src/mention.rs`, `src/upload.rs`.

The zca-js license text, verbatim:

> MIT License
>
> Copyright (c) 2024 - 2025 RFS-ADRENO, truong9c2208, JustKemForFun
>
> Permission is hereby granted, free of charge, to any person obtaining a copy
> of this software and associated documentation files (the "Software"), to deal
> in the Software without restriction, including without limitation the rights
> to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
> copies of the Software, and to permit persons to whom the Software is
> furnished to do so, subject to the following conditions:
>
> The above copyright notice and this permission notice shall be included in all
> copies or substantial portions of the Software.
>
> THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
> IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
> FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
> AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
> LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
> OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
> SOFTWARE.
