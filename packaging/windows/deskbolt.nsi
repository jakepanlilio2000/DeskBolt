Unicode True
SetCompressor /SOLID lzma

!ifndef VERSION
  !define VERSION "0.1.0-beta1"
!endif

!ifndef VI_VERSION
  !searchparse /noerrors "${VERSION}-" "" _DESKBOLT_SEMVER "-"
  !ifdef _DESKBOLT_SEMVER
    !define VI_VERSION "${_DESKBOLT_SEMVER}.0"
  !else
    !define VI_VERSION "${VERSION}.0"
  !endif
!endif

!ifndef DIST_DIR
  !define DIST_DIR "..\..\target\release"
!endif

!ifndef ROOT_DIR
  !define ROOT_DIR "..\.."
!endif

Name "DeskBolt"
OutFile "DeskBolt-v${VERSION}-windows-x64-setup.exe"
InstallDir "$PROGRAMFILES64\DeskBolt"
InstallDirRegKey HKLM "Software\DeskBolt" "InstallDir"
RequestExecutionLevel admin
BrandingText "StackSync Solutions"

VIProductVersion "${VI_VERSION}"
VIFileVersion "${VI_VERSION}"
VIAddVersionKey "CompanyName" "StackSync Solutions"
VIAddVersionKey "ProductName" "DeskBolt"
VIAddVersionKey "FileDescription" "DeskBolt Remote Desktop Setup"
VIAddVersionKey "FileVersion" "${VERSION}"
VIAddVersionKey "ProductVersion" "${VERSION}"
VIAddVersionKey "LegalCopyright" "Copyright (c) StackSync Solutions (MPL-2.0)"

!include "MUI2.nsh"
!include "nsDialogs.nsh"
!include "Sections.nsh"
!include "LogicLib.nsh"
!include "x64.nsh"
!include "FileFunc.nsh"

Var ServerPassword
Var ServerPort
Var ConfigDialog
Var PasswordEdit
Var ConfirmEdit
Var PortEdit
Var PortValid

!define MUI_ABORTWARNING
!define MUI_FINISHPAGE_RUN "$INSTDIR\deskbolt-viewer.exe"
!define MUI_FINISHPAGE_RUN_TEXT "Launch DeskBolt Viewer"
!define MUI_FINISHPAGE_RUN_FUNCTION LaunchViewerIfInstalled

!insertmacro MUI_PAGE_WELCOME
!insertmacro MUI_PAGE_LICENSE "${ROOT_DIR}\LICENSE"
!insertmacro MUI_PAGE_COMPONENTS
!insertmacro MUI_PAGE_DIRECTORY
Page custom ServerConfigPageCreate ServerConfigPageLeave
!insertmacro MUI_PAGE_INSTFILES
!insertmacro MUI_PAGE_FINISH

!insertmacro MUI_UNPAGE_CONFIRM
!insertmacro MUI_UNPAGE_INSTFILES

!insertmacro MUI_LANGUAGE "English"

; ==============================================================================
; Installer Components
; ==============================================================================
Section "DeskBolt Viewer" SecViewer
  SetRegView 64
  SetShellVarContext all
  SetOutPath "$INSTDIR"

  File "${DIST_DIR}\deskbolt-viewer.exe"
  File "${ROOT_DIR}\LICENSE"
  File "${ROOT_DIR}\README.md"

  CreateDirectory "$SMPROGRAMS\DeskBolt"
  CreateShortcut "$SMPROGRAMS\DeskBolt\DeskBolt Viewer.lnk" "$INSTDIR\deskbolt-viewer.exe"
  CreateShortcut "$SMPROGRAMS\DeskBolt\Uninstall DeskBolt.lnk" "$INSTDIR\uninstall.exe"
SectionEnd

Section /o "Desktop Shortcut (Viewer)" SecDesktop
  SetShellVarContext all
  ${If} ${FileExists} "$INSTDIR\deskbolt-viewer.exe"
    CreateShortcut "$DESKTOP\DeskBolt Viewer.lnk" "$INSTDIR\deskbolt-viewer.exe"
  ${EndIf}
SectionEnd

Section /o "DeskBolt Server (Background Service)" SecServer
  SetRegView 64
  SetShellVarContext all
  SetOutPath "$INSTDIR"

  ; Stop any previously running service before replacing the executable.
  nsExec::ExecToStack 'net stop DeskBoltSvc'
  Pop $0
  Pop $1

  File "${DIST_DIR}\deskbolt-server.exe"
  File "${ROOT_DIR}\LICENSE"
  File "${ROOT_DIR}\README.md"

  CreateDirectory "$SMPROGRAMS\DeskBolt"
  CreateShortcut "$SMPROGRAMS\DeskBolt\Uninstall DeskBolt.lnk" "$INSTDIR\uninstall.exe"

  ; Configure inbound Windows Defender Firewall rule for TCP port $ServerPort.
  nsExec::ExecToStack 'netsh advfirewall firewall delete rule name="DeskBolt Remote Host"'
  Pop $0
  Pop $1
  nsExec::ExecToStack 'netsh advfirewall firewall add rule name="DeskBolt Remote Host" dir=in action=allow protocol=TCP localport=$ServerPort program="$INSTDIR\deskbolt-server.exe" enable=yes profile=any'
  Pop $0
  Pop $1
  ${If} $0 != 0
    DetailPrint "Windows Firewall rule configuration exited with code $0: $1"
  ${EndIf}

  ; Set host password and port via process environment to avoid command-line exposure.
  ${If} $ServerPassword != ""
    System::Call 'Kernel32::SetEnvironmentVariableW(w "DESKBOLT_INIT_PASSWORD", w "$ServerPassword")'
    nsExec::ExecToStack '"$INSTDIR\deskbolt-server.exe" --set-password --password-env DESKBOLT_INIT_PASSWORD --port $ServerPort'
    Pop $0
    Pop $1
    System::Call 'Kernel32::SetEnvironmentVariableW(w "DESKBOLT_INIT_PASSWORD", i 0)'
    StrCpy $ServerPassword ""
    ${If} $0 != 0
      DetailPrint "Host password setup failed (exit code $0): $1"
      SetErrorLevel $0
      ${IfNot} ${Silent}
        MessageBox MB_OK|MB_ICONSTOP "Failed to configure DeskBolt Server password (exit code $0).$\r$\n$1"
      ${EndIf}
      Abort
    ${EndIf}
  ${EndIf}

  ; Register and start DeskBoltSvc.
  nsExec::ExecToStack '"$INSTDIR\deskbolt-server.exe" --install'
  Pop $0
  Pop $1
  ${If} $0 != 0
    DetailPrint "Service installation failed (exit code $0): $1"
    SetErrorLevel $0
    ${IfNot} ${Silent}
      MessageBox MB_OK|MB_ICONSTOP "Failed to install DeskBolt service (exit code $0).$\r$\n$1"
    ${EndIf}
    Abort
  ${EndIf}

  nsExec::ExecToStack 'net start DeskBoltSvc'
  Pop $0
  Pop $1
  ${If} $0 != 0
    DetailPrint "Failed to start DeskBoltSvc (exit code $0): $1"
    SetErrorLevel $0
    ${IfNot} ${Silent}
      MessageBox MB_OK|MB_ICONSTOP "DeskBolt service was installed but could not start (exit code $0).$\r$\n$1"
    ${EndIf}
    Abort
  ${EndIf}
SectionEnd

Section -PostInstall
  SetRegView 64
  SetShellVarContext all

  WriteUninstaller "$INSTDIR\uninstall.exe"

  WriteRegStr HKLM "Software\DeskBolt" "InstallDir" "$INSTDIR"
  WriteRegStr HKLM "Software\DeskBolt" "Port" "$ServerPort"

  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\DeskBolt" "DisplayName" "DeskBolt"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\DeskBolt" "DisplayVersion" "${VERSION}"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\DeskBolt" "Publisher" "StackSync Solutions"
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\DeskBolt" "UninstallString" '"$INSTDIR\uninstall.exe"'
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\DeskBolt" "QuietUninstallString" '"$INSTDIR\uninstall.exe" /S'
  WriteRegStr HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\DeskBolt" "InstallLocation" "$INSTDIR"
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\DeskBolt" "NoModify" 1
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\DeskBolt" "NoRepair" 1

  ${GetSize} "$INSTDIR" "/S=0K" $0 $1 $2
  IntFmt $0 "0x%08X" $0
  WriteRegDWORD HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\DeskBolt" "EstimatedSize" $0
SectionEnd

!insertmacro MUI_FUNCTION_DESCRIPTION_BEGIN
  !insertmacro MUI_DESCRIPTION_TEXT ${SecViewer} "Install the DeskBolt Viewer client for connecting to remote machines."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecDesktop} "Create a DeskBolt Viewer shortcut on the public Desktop."
  !insertmacro MUI_DESCRIPTION_TEXT ${SecServer} "Install and start the DeskBolt Remote Host background Windows service and firewall rule."
!insertmacro MUI_FUNCTION_DESCRIPTION_END

; ==============================================================================
; Initialization, Silent CLI Options & Custom Server Configuration Page
; ==============================================================================
Function ValidatePort
  ; Input: $R1 (string). Output: $PortValid (1 if integer in 1..65535, else 0).
  StrCpy $PortValid 0
  StrLen $R2 $R1
  ${If} $R2 < 1
  ${OrIf} $R2 > 5
    Return
  ${EndIf}

  StrCpy $R3 0
  ${DoWhile} $R3 < $R2
    StrCpy $R4 $R1 1 $R3
    ${If} $R4 S< "0"
    ${OrIf} $R4 S> "9"
      Return
    ${EndIf}
    IntOp $R3 $R3 + 1
  ${Loop}

  IntOp $R5 $R1 + 0
  ${If} $R5 >= 1
  ${AndIf} $R5 <= 65535
    StrCpy $PortValid 1
  ${EndIf}
FunctionEnd

Function .onInit
  SetRegView 64

  ${IfNot} ${RunningX64}
    SetErrorLevel 216
    ${IfNot} ${Silent}
      MessageBox MB_OK|MB_ICONSTOP "DeskBolt requires 64-bit Windows."
    ${EndIf}
    Abort
  ${EndIf}

  StrCpy $ServerPassword ""
  StrCpy $ServerPort "4899"

  ${GetParameters} $R0

  ClearErrors
  ${GetOptions} $R0 "/VIEWER=" $R1
  ${IfNot} ${Errors}
    ${If} $R1 == "0"
      !insertmacro UnselectSection ${SecViewer}
      !insertmacro UnselectSection ${SecDesktop}
    ${ElseIf} $R1 == "1"
      !insertmacro SelectSection ${SecViewer}
    ${EndIf}
  ${EndIf}

  ClearErrors
  ${GetOptions} $R0 "/SERVER=" $R1
  ${IfNot} ${Errors}
    ${If} $R1 == "1"
      !insertmacro SelectSection ${SecServer}
    ${ElseIf} $R1 == "0"
      !insertmacro UnselectSection ${SecServer}
    ${EndIf}
  ${EndIf}

  ClearErrors
  ${GetOptions} $R0 "/PASSWORD=" $R1
  ${IfNot} ${Errors}
    StrCpy $ServerPassword "$R1"
  ${EndIf}

  ClearErrors
  ${GetOptions} $R0 "/PORT=" $R1
  ${IfNot} ${Errors}
    Call ValidatePort
    ${If} $PortValid != 1
      SetErrorLevel 87
      ${IfNot} ${Silent}
        MessageBox MB_OK|MB_ICONSTOP "Invalid /PORT value '$R1'. Expected a port between 1 and 65535."
      ${EndIf}
      Abort
    ${EndIf}
    StrCpy $ServerPort "$R1"
  ${EndIf}

  ${If} ${Silent}
    ${IfNot} ${SectionIsSelected} ${SecViewer}
    ${AndIfNot} ${SectionIsSelected} ${SecServer}
      SetErrorLevel 87
      Abort
    ${EndIf}

    ${If} ${SectionIsSelected} ${SecServer}
    ${AndIf} $ServerPassword == ""
      ReadEnvStr $R2 "ProgramData"
      ${If} $R2 == ""
        StrCpy $R2 "C:\ProgramData"
      ${EndIf}
      ${IfNot} ${FileExists} "$R2\DeskBolt\config.toml"
        SetErrorLevel 87
        Abort
      ${EndIf}
    ${EndIf}
  ${EndIf}
FunctionEnd

Function .onSelChange
  ${IfNot} ${SectionIsSelected} ${SecViewer}
    !insertmacro UnselectSection ${SecDesktop}
  ${EndIf}
FunctionEnd

Function ServerConfigPageCreate
  ${IfNot} ${SectionIsSelected} ${SecServer}
    Abort
  ${EndIf}
  ${If} $ServerPassword != ""
    Abort
  ${EndIf}

  !insertmacro MUI_HEADER_TEXT "DeskBolt Server Configuration" "Choose the host access password and TCP listening port."

  nsDialogs::Create 1018
  Pop $ConfigDialog
  ${If} $ConfigDialog == error
    Abort
  ${EndIf}

  ${NSD_CreateLabel} 0 0 100% 14u "Host Access Password:"
  Pop $0
  ${NSD_CreatePassword} 0 16u 100% 14u ""
  Pop $PasswordEdit

  ${NSD_CreateLabel} 0 38u 100% 14u "Confirm Password:"
  Pop $0
  ${NSD_CreatePassword} 0 54u 100% 14u ""
  Pop $ConfirmEdit

  ${NSD_CreateLabel} 0 78u 100% 14u "TCP Listening Port (default 4899):"
  Pop $0
  ${NSD_CreateNumber} 0 94u 80u 14u "$ServerPort"
  Pop $PortEdit

  nsDialogs::Show
FunctionEnd

Function ServerConfigPageLeave
  ${NSD_GetText} $PasswordEdit $0
  ${NSD_GetText} $ConfirmEdit $1
  ${NSD_GetText} $PortEdit $R1

  ${If} $0 == ""
    MessageBox MB_OK|MB_ICONEXCLAMATION "Please enter a host access password."
    Abort
  ${EndIf}

  ${If} $0 != $1
    StrCpy $0 ""
    StrCpy $1 ""
    MessageBox MB_OK|MB_ICONEXCLAMATION "Passwords do not match. Please re-enter them."
    Abort
  ${EndIf}

  Call ValidatePort
  ${If} $PortValid != 1
    StrCpy $0 ""
    StrCpy $1 ""
    MessageBox MB_OK|MB_ICONEXCLAMATION "Please enter a valid TCP port between 1 and 65535."
    Abort
  ${EndIf}

  StrCpy $ServerPassword "$0"
  StrCpy $ServerPort "$R1"
  StrCpy $0 ""
  StrCpy $1 ""
FunctionEnd

Function LaunchViewerIfInstalled
  ${If} ${FileExists} "$INSTDIR\deskbolt-viewer.exe"
    Exec '"$INSTDIR\deskbolt-viewer.exe"'
  ${EndIf}
FunctionEnd

; ==============================================================================
; Uninstaller
; ==============================================================================
Function un.onInit
  SetRegView 64
FunctionEnd

Section "Uninstall"
  SetRegView 64
  SetShellVarContext all

  ${If} ${FileExists} "$INSTDIR\deskbolt-server.exe"
    nsExec::ExecToStack 'net stop DeskBoltSvc'
    Pop $0
    Pop $1

    nsExec::ExecToStack '"$INSTDIR\deskbolt-server.exe" --uninstall'
    Pop $0
    Pop $1
    ${If} $0 != 0
    ${AndIf} $0 != 1060
      DetailPrint "Service uninstall returned Win32 exit code $0: $1"
    ${EndIf}

    nsExec::ExecToStack 'netsh advfirewall firewall delete rule name="DeskBolt Remote Host"'
    Pop $0
    Pop $1
  ${EndIf}

  Delete "$INSTDIR\deskbolt-viewer.exe"
  Delete "$INSTDIR\deskbolt-server.exe"
  Delete "$INSTDIR\LICENSE"
  Delete "$INSTDIR\README.md"
  Delete "$INSTDIR\uninstall.exe"
  SetOutPath "$TEMP"
  RMDir "$INSTDIR"

  Delete "$DESKTOP\DeskBolt Viewer.lnk"
  RMDir /r "$SMPROGRAMS\DeskBolt"

  DeleteRegKey HKLM "Software\Microsoft\Windows\CurrentVersion\Uninstall\DeskBolt"
  DeleteRegKey HKLM "Software\DeskBolt"

  ${IfNot} ${Silent}
    MessageBox MB_YESNO|MB_ICONQUESTION "Remove DeskBolt host configuration, TLS certificates, and saved address book data from this machine?" IDNO _keep_user_data
    SetShellVarContext current
    RMDir /r "$APPDATA\DeskBolt"
    ClearErrors
    ReadEnvStr $0 "ProgramData"
    ${If} ${Errors}
    ${OrIf} $0 == ""
      StrCpy $0 "C:\ProgramData"
    ${EndIf}
    RMDir /r "$0\DeskBolt"
  ${EndIf}

  _keep_user_data:
SectionEnd
