# Module API v1

## Назначение и границы

Module API v1 — статический контракт метаданных встроенных модулей и команд Lavis. Он даёт русскоязычной Help v2 единый источник для списка модулей, карточек команд, примеров, происхождения, уровня риска и возможностей. Реестр компилируется вместе с программой; его можно проверять без Telegram.

Это **не** protocol внешних процессов. Внешний runtime, manifest schema 2/3 и `.lmod` installer описаны в [Module API v2/v3](module-api-v2.md), [External Modules](external-modules.md) и [Packaging `.lmod`](lmod-packaging.md). `ModuleOrigin::External` внутри v1 остаётся только валидируемым описанием происхождения для Help и не загружает код самостоятельно.

## Типы метаданных

### Модуль

`ModuleId` — стабильный типовой идентификатор встроенного модуля: `Core`, `System` или `Aliases`.

`ModuleOrigin` описывает происхождение:

- `Builtin` — модуль собран вместе с Lavis;
- `External { author, version, source }` — отображаемые автор, версия и источник внешнего происхождения. Все три строки обязаны быть непустыми, ограниченными по длине, без управляющих и bidi-символов; `source` не может быть локальным абсолютным путём или `file:`-URL.

`ModuleCapability` — заявленная возможность встроенного модуля:

- `TelegramRpc` — обращение к Telegram RPC;
- `PersistentStateRead` — чтение постоянного локального состояния;
- `PersistentStateWrite` — изменение постоянного локального состояния;
- `HostInformation` — получение информации о хосте;
- `RestrictedProcess` — запуск заранее определённого ограниченного процесса;
- `Network` — сетевая операция.

Этот enum относится к статическому built-in registry и не совпадает с capability strings manifest внешних модулей.

`ModuleSpec` содержит `id`, каноническое ASCII-имя `name`, русское `description_ru`, значок `icon`, `origin`, срез `capabilities`, а также флаги `unloadable` и `replaceable`. В текущем скомпилированном реестре оба флага у всех модулей `false`.

### Команда

`CommandKind` — типовой вид встроенной команды: `Ping`, `Stats`, `Help`, `Fastfetch`, `Alias`, `Prefix`, `Modules`, `Setup`, `Lm` или `Reboot`.

`CommandRisk` описывает максимальный характер действия:

- `ReadOnly` — чтение без сохранения изменений;
- `PersistentStateChange` — изменение локального постоянного состояния;
- `RestrictedProcess` — строго ограниченный вызов известной программы;
- `ArbitraryProcess` — произвольный процесс;
- `Privileged` — чувствительное действие с Telegram/account state;
- `ExternalCodeInstall` — проверка и сохранение внешнего исполняемого кода без автоматического запуска.

`setup` и `reboot` имеют риск `Privileged`; `lm` — `ExternalCodeInstall`; `fastfetch` — `RestrictedProcess`. Риск описывает пользовательский эффект команды, а не даёт разрешение обходить runtime-проверки.

`CommandDefinition` связывает `kind` с каноническим `name`, синтаксисом `usage`, русскими `summary_ru` и `description_ru`, списком безпрефиксных `examples`, `risk`, `icon`, флагом `aliasable` и владельцем `module: ModuleId`. Каждый пример начинается с имени канонической команды, а UI добавляет активный префикс.

## Статические реестры и запросы

Единственные источники built-in metadata — compile-time реестры. Для чтения используйте:

- `modules()` и `module_by_id(ModuleId)`;
- `module_by_name(&str)`;
- `commands()` и `command_by_kind(CommandKind)`;
- `command_by_name(&str)`;
- `commands_for_module(ModuleId)`;
- `module_for_command(&CommandDefinition)`.

Поиск по имени модуля и команды нечувствителен к ASCII-регистру. Инварианты реестра: идентификаторы и имена уникальны, каждое имя непусто, каждая команда принадлежит зарегистрированному модулю, каждый модуль содержит команду, а `commands_for_module` покрывает команды ровно по одному разу. Описания, значки и примеры обязательны.

## Help v2, внешние модули и псевдонимы

Help v2 использует статические поля v1 для built-in cards. Внешние descriptors (включая disabled) и active command refs поступают из external runtime отдельным каналом и не добавляются в `MODULE_SPECS`/`COMMAND_SPECS`.

Порядок тем справки: встроенная каноническая команда → активная внешняя namespaced-команда → псевдоним → встроенный модуль → обнаруженный внешний модуль → неизвестная тема.

Порядок исполнения резервирует встроенные canonical names. Внешние команды вызываются namespaced-именем `module-id.command-name`; schema 3 default command может использовать короткое имя module ID; затем проверяются псевдонимы.

Команда `lm` имеет специализированную карточку Help, потому что generic metadata недостаточно для описания Saved Messages, `.lmod`, inspection plan, ApprovalId, TTL, disabled-after-install semantics и persistent enable state. `lm list` и `lm info <id>` читают состояние; `lm enable <id>` и `lm disable <id>` меняют его только для следующего запуска. Горячая загрузка отсутствует.

`reboot` безопасно перезапускает только процесс Lavis, а не операционную систему. Он редактирует то же сообщение с командой сначала в «♻️ Lavis перезапускается…», а после успешного запуска — в «✅ Lavis перезагрузился» с целым временем перезапуска в секундах с усечением дробной части; отдельное сообщение не создаётся. Как и изменяющие состояние варианты `lm`, он принимается только в новом собственном сообщении «Сохранённых сообщений»; редактированные сообщения не подходят.

## Как зарегистрировать встроенную команду

Регистрация выполняется одним согласованным изменением:

1. Добавьте `ModuleId` при появлении нового встроенного модуля и его `ModuleSpec`.
2. Добавьте `CommandKind` и полный `CommandDefinition`.
3. Добавьте вариант `Action` для типизированного намерения команды.
4. Добавьте сопоставление `CommandKind` с `Action` в `dispatch`.
5. Добавьте отдельный parser аргументов в типизированный request.
6. Добавьте выполнение `Action` в runtime.
7. Добавьте тесты реестра, parser, dispatch, runtime и Help v2.
8. Обновите README и соответствующий документ в `docs/`.

Не разбирайте префикс повторно. В `examples` пишите `ping`, `help system` или `lm list`, а не варианты с `,`.

## Безопасность built-in команд

- `RestrictedProcess` запускает только фиксированную программу с типизированными/ограниченными аргументами, bounded output и timeout; shell не используется.
- `Privileged` требует отдельной строгой state machine и безопасного отображения ошибок.
- `ExternalCodeInstall` не запускает устанавливаемый код, не включает модуль и не перезаписывает существующий target.
- Transport/media данные не должны протекать в статические metadata types.
- Ошибки не раскрывают секреты, локальные пути, содержимое untrusted files или raw stderr.

## Тесты

Тестируйте уникальность и полноту реестров, соответствие риска поведению, поиск по именам, связь команда↔модуль, порядок Help/runtime resolution, безпрефиксность примеров и строгий разбор аргументов.

Для `lm` дополнительно проверяются canonical ApprovalId syntax, edited-message policy, Saved Messages gating и специализированная Help card. Архивные/installer invariants тестируются в external module subsystem, а не в v1 registry.

Перед PR выполните:

```bash
nix develop
cargo fmt --check
cargo check --all-targets --all-features
cargo clippy --all-targets --all-features -- -D warnings
cargo test --all-targets --all-features
nix flake check
nix build
```

## Текущее состояние

Production registry v1 по-прежнему содержит только скомпилированные модули `Core`, `System` и `Aliases`. Поддержка внешнего кода существует параллельно через external descriptors, manager, schema 2/3 protocol и `.lmod` installer. Эти два уровня нельзя смешивать дублирующими реестрами или превращать `ModuleOrigin::External` в обход внешнего manifest/runtime validation.
