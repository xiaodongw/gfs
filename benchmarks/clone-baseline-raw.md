<!-- ripgrep: ripgrep 15.2.0 -->
| repo | variant | wall s | .git MiB | work MiB | objects MiB | files | rg s | rg hits |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: |
| vscode   | full                   |    57.0 |    1376.8 |     226.6 |    1374.0 |    16862 |   0.04 |    901 |
| vscode   | shallow-depth1         |     3.8 |      51.7 |     226.6 |      49.4 |    16862 |   0.04 |    901 |
| vscode   | blobless               |    25.8 |     369.9 |     226.6 |     367.1 |    16862 |   0.04 |    901 |
| vscode   | treeless               | FAILED: fatal: filter 'tree' not supported |
| vscode   | shallow+blobless       |     4.3 |      53.2 |     226.6 |      50.9 |    16862 |   0.04 |    901 |
| vscode   | sparse:src/vs/editor   |    20.9 |     322.6 |      12.3 |     319.8 |     1330 |   0.01 |    108 |
| vscode   | bare-full              |    53.6 |    1374.4 |         0 |    1374.0 |        0 |   0.01 |      1 |
| rust     | full                   |    76.9 |    1039.9 |     212.1 |    1032.8 |    61296 |   0.07 |    207 |
| rust     | shallow-depth1         |     7.7 |      55.9 |     212.1 |      48.7 |    61296 |   0.07 |    207 |
| rust     | blobless               |    50.7 |     488.0 |     212.1 |     480.9 |    61296 |   0.08 |    207 |
| rust     | treeless               | FAILED: fatal: remote error: filter 'tree' not supported |
| rust     | shallow+blobless       |     9.5 |      60.9 |     212.1 |      53.8 |    61296 |   0.06 |    207 |
| rust     | sparse:compiler        |    47.4 |     445.2 |      34.2 |     438.0 |     2901 |   0.01 |      0 |
| rust     | bare-full              |    80.6 |    1032.8 |         0 |    1032.8 |        0 |   0.01 |      1 |
| linux    | full                   |   383.1 |    6546.9 |    1540.4 |    6537.1 |    94751 |   0.13 |   3155 |
| linux    | shallow-depth1         |    14.9 |     291.2 |    1540.4 |     281.5 |    94751 |   0.13 |   3155 |
| linux    | blobless               |   181.4 |    2313.3 |    1540.4 |    2303.4 |    94751 |   0.13 |   3155 |
| linux    | treeless               | FAILED: fatal: remote error: filter 'tree' not supported |
| linux    | shallow+blobless       |    19.5 |     298.7 |    1540.4 |     288.9 |    94751 |   0.14 |   3155 |
| linux    | sparse:drivers/net     |   167.7 |    2060.7 |     144.8 |    2050.7 |     6813 |   0.03 |    401 |
| linux    | bare-full              |   373.7 |    6537.2 |         0 |    6537.1 |        0 |   0.01 |      1 |
