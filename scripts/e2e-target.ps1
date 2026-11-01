param([string]$OutPath, [int]$CloseAfterMs)
Add-Type -AssemblyName System.Windows.Forms
$script:form = New-Object System.Windows.Forms.Form
$form.Text = "Offhand E2E target"
$form.TopMost = $true
$form.Width = 640
$form.Height = 200
$script:box = New-Object System.Windows.Forms.TextBox
$box.Multiline = $true
$box.Dock = 'Fill'
$form.Controls.Add($box)
$script:timer = New-Object System.Windows.Forms.Timer
$timer.Interval = $CloseAfterMs
$timer.Add_Tick({ [IO.File]::WriteAllText($OutPath, $script:box.Text); $script:form.Close() })
$form.Add_Shown({ $script:form.Activate(); $script:box.Focus(); $script:timer.Start() })
[void]$form.ShowDialog()
