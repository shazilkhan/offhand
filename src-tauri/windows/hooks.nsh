; Offhand keeps its data in %LOCALAPPDATA%\Offhand, which is also the default
; per-user install folder. The app goes beside other per-user apps instead,
; unless someone picked a folder of their own.
!macro NSIS_HOOK_PREINSTALL
  ${If} $INSTDIR == "$LOCALAPPDATA\${PRODUCTNAME}"
    StrCpy $INSTDIR "$LOCALAPPDATA\Programs\${PRODUCTNAME}"
    SetOutPath $INSTDIR
  ${EndIf}
!macroend

; The start-with-Windows entry goes with the app. An upgrade uninstalls the old
; version too; the new one writes the entry back when it starts.
!macro NSIS_HOOK_POSTUNINSTALL
  DeleteRegValue HKCU "Software\Microsoft\Windows\CurrentVersion\Run" "${PRODUCTNAME}"
!macroend
