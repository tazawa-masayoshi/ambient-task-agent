# Skill Templates

各リポジトリの `.claude/skills/` にコピーするテンプレート。

## 使い方

```bash
# hikken_schedule の場合
cp -r config/skill-templates/hikken_schedule/ops/ /path/to/hikken_schedule/.claude/skills/ops/
```

## 構造

```
skill-templates/
└── hikken_schedule/
    └── ops/
        ├── SKILL.md          # → 各リポジトリの既存 ops.md を移動
        ├── gotchas.md        # よくある失敗パターン（自動読み込み）
        └── references/       # 詳細リファレンス（自動読み込み）
            └── *.md
```

`read_ops_skills()` が SKILL.md と同フォルダの `gotchas.md`、`references/*.md` を自動結合する。
