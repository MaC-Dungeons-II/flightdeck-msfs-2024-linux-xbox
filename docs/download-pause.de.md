# Spieldownload pausieren oder erneut versuchen

[English](download-pause.md)

Während der Installation stoppt **Download pausieren** den eigenen
Xodus-Downloadprozess. **Download fortsetzen** startet ihn im selben privaten
Installationsordner. Flightdeck hält die Einrichtung währenddessen reserviert.

Ab **0.2.21** erscheint nach einem fehlgeschlagenen Spieldownload oder einer
fehlgeschlagenen Microsoft-Anmeldung **Download erneut versuchen**. Der
Hintergrunddienst wartet auf deine Auswahl und behält den Arbeitsordner.
Vollständig geladene Dateien werden erneut geprüft und weiterverwendet;
unvollständige oder veränderte Dateien werden erneut geladen. Dasselbe gilt
für Downloads unter **Updates**.

**Abbrechen** gibt die Einrichtung frei und behält ihre Dateien. Ein
abgebrochener Job lässt sich anschließend nicht fortsetzen. Solange ein
Download pausiert ist oder auf einen erneuten Versuch wartet, bleiben andere
Installationen und Spielstarts gesperrt.

## Welche Daten bleiben erhalten?

Jeder fortgesetzte Download prüft abgeschlossene Dateien anhand ihrer
gespeicherten SHA-256-Prüfsumme und der exakten Länge. Bis zu vier Dateien
können gleichzeitig geladen werden. Ihre unfertigen Teile beginnen beim
Fortsetzen neu; Pause bedeutet deshalb keine bytegenaue Wiederaufnahme.

Flightdeck zeigt empfangene und geprüfte Daten sowie den Gesamtumfang, sofern
die Komponenten numerischen Fortschritt unterstützen. Wiederholte Transfers
erhöhen die Paketgröße nicht. Nach einer Unterbrechung kann der angezeigte
Teilfortschritt sinken, wenn eine unfertige Datei neu beginnt. Die Angaben
beziehen sich auf das Store-Basispaket; Runner, Caches und zusätzliche Inhalte
im Simulator benötigen weiteren Platz.

## Anmeldung und Fehler

Ist die Anmeldung abgelaufen, öffnet sich das Microsoft-Fenster erneut.
Flightdeck versucht die automatische Anmeldung einmal pro Fortsetzung oder
ausdrücklichem Retry. Bei wiederholtem Fehler wartet es auf deine Auswahl.
Kaufberechtigung und Microsoft-Dienste müssen den Download weiterhin erlauben.
Hat sich die beim Update festgelegte Paketrevision geändert, musst du das
Update erneut prüfen. Dateien verschiedener Revisionen werden nicht vermischt.

Anmelde- und Downloadcodes sowie Edition, Fehlerphase und Versuchszahl stehen
unter **Diagnose → Problem melden** auch vor der ersten fertigen Installation.
Diese feste Zusammenfassung bleibt nach einem Dienstneustart erhalten. Rohe
Prozessausgaben, Kontodaten, Zugangstokens und lokale Pfade werden nicht erfasst.
Ältere Berichte erhalten fehlende Fehlercodes nicht rückwirkend.

## Grenzen

Lass den Flightdeck-Hintergrunddienst laufen. Das Desktopfenster darfst du
schließen und erneut öffnen; Sprache und Ansicht lassen sich wechseln.
Nach Dienstende, Abmelden oder Linux-Neustart wird der aktive Downloadjob noch
nicht wiederhergestellt. Die gespeicherte Diagnose ersetzt diesen Job nicht.
Auch bereits beendete Jobs älterer Flightdeck-Versionen lassen sich über den
neuen Knopf nicht wiederherstellen.

Ältere Komponenten ohne geprüfte Resume-Unterstützung bieten keine erneuten
Versuche mit Dateiwiederverwendung an. Die technischen Voraussetzungen und
Prüfungen stehen im [englischen Protokollleitfaden](download-pause.md#component-contract).
