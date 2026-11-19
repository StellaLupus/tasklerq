# Встроенные ресурсы консоли

Файлы этого каталога встраиваются в бинарник (`include_bytes!`) и отдаются
консолью без обращения к внешним адресам (design 0013, решение 12).

## console.css

- Стили консоли: формы, сообщения и навигация. Тёмная тема по умолчанию
  (`color-scheme: dark`), цвета и размеры — переменные в `:root`.
- Внешних шрифтов и `url(...)` на внешние адреса нет: консоль работает в LAN
  без интернета.

## htmx.min.js

- Версия: htmx 2.0.4, `dist/htmx.min.js` пакета `htmx.org` без изменений.
- Источник: `https://unpkg.com/htmx.org@2.0.4/dist/htmx.min.js` (сверено с копией
  `cdn.jsdelivr.net/npm/htmx.org@2.0.4/dist/htmx.min.js`).
- SRI: `sha384-HGfztofotfshcF7+8n44JQL2oJmowVChPTg48S+jvZoztPfvwD79OC/LTtG6dMp+`
  (совпадает с опубликованным в документации htmx).
- Лицензия: Zero-Clause BSD (0BSD), https://github.com/bigskysoftware/htmx/blob/master/LICENSE

Обновление: заменить файл целиком на новую закреплённую версию, пересчитать
`openssl dgst -sha384 -binary htmx.min.js | openssl base64 -A` и сверить с
опубликованным значением, обновить этот раздел.
