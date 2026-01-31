use console::style;
use hashbrown::HashMap;
use std::{
    fmt::{Display, Formatter, Result as FmtResult},
    path::PathBuf,
};

#[derive(Clone)]
pub struct ConfigGroup {
    location: PathBuf,
    files: Vec<PathBuf>,
}

impl ConfigGroup {
    pub fn new(location: PathBuf, mut files: Vec<PathBuf>) -> Self {
        files.sort();
        Self { location, files }
    }

    fn location_label(&self) -> String {
        let name = self
            .location
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or_default();
        if name.is_empty() {
            self.location.to_string_lossy().to_string()
        } else {
            name.to_string()
        }
    }

    fn join_names(&self) -> String {
        let mut names: Vec<String> = self
            .files
            .iter()
            .map(|path| {
                path.file_stem()
                    .and_then(|stem| stem.to_str())
                    .unwrap_or_default()
                    .to_string()
            })
            .collect();
        names.sort();
        names.join(" ")
    }
}

fn config_n_files_string(n: usize) -> String {
    format!(
        "{} {} file{}",
        style("Config").blue().bold(),
        n,
        if n == 1 { "" } else { "s" }
    )
}

pub struct ConfigDisplay {
    files: Vec<PathBuf>,
}

impl ConfigDisplay {
    pub fn new(mut files: Vec<PathBuf>) -> Self {
        files.sort();
        Self { files }
    }

    pub fn lines(&self) -> Vec<String> {
        let total = self.files.len();
        let groups = self.build_groups();

        if groups.len() == total {
            let mut lines = Vec::with_capacity(total + 2);
            lines.push(format!(
                "{} ({} locations)",
                config_n_files_string(total),
                total
            ));
            for path in self.files.iter() {
                lines.push(format!("  {}", path.to_string_lossy()));
            }
            return lines;
        }

        if let Some((dominant_idx, dominant_count)) = self.dominant_group(&groups) {
            let main_ratio = if total == 0 {
                0.0
            } else {
                dominant_count as f32 / total as f32
            };
            let external_count = total.saturating_sub(dominant_count);
            if main_ratio >= 0.75 && external_count <= 5 {
                let main = &groups[dominant_idx];
                let mut lines = Vec::new();
                lines.push(config_n_files_string(total));
                lines.push(format!(
                    "    {} ({})",
                    main.location.to_string_lossy(),
                    main.files.len()
                ));
                lines.push(format!("        {}", main.join_names()));
                let suffix = if external_count == 1 { "file" } else { "files" };
                lines.push(format!("    +{} external {}", external_count, suffix));
                return lines;
            }
        }

        if groups.len() == 1 {
            let group = &groups[0];
            return vec![
                config_n_files_string(total),
                format!("    {}", group.location.to_string_lossy()),
                format!("        {}", group.join_names()),
            ];
        }

        let mut lines = Vec::new();
        lines.push(format!(
            "{} in {} locations",
            config_n_files_string(total),
            groups.len()
        ));
        for group in groups {
            lines.push(format!(
                "    {} ({})",
                group.location_label(),
                group.files.len()
            ));
            lines.push(format!("    {}", group.location.to_string_lossy()));
            lines.push(format!("        {}", group.join_names()));
        }
        while lines.last().map(|line| line.is_empty()).unwrap_or(false) {
            lines.pop();
        }
        lines
    }

    fn build_groups(&self) -> Vec<ConfigGroup> {
        let mut groups_map: HashMap<PathBuf, Vec<PathBuf>> = HashMap::new();
        for path in self.files.iter() {
            let key = path.parent().map(PathBuf::from).unwrap_or_default();
            groups_map.entry(key).or_default().push(path.clone());
        }
        let mut groups: Vec<ConfigGroup> = groups_map
            .into_iter()
            .map(|(location, files)| ConfigGroup::new(location, files))
            .collect();
        groups.sort_by(|a, b| a.location.cmp(&b.location));
        groups
    }

    fn dominant_group(&self, groups: &[ConfigGroup]) -> Option<(usize, usize)> {
        let mut dominant_idx = None;
        let mut dominant_count = 0;
        for (idx, group) in groups.iter().enumerate() {
            if group.files.len() > dominant_count {
                dominant_count = group.files.len();
                dominant_idx = Some(idx);
            }
        }
        dominant_idx.map(|idx| (idx, dominant_count))
    }
}

impl Display for ConfigDisplay {
    fn fmt(&self, f: &mut Formatter<'_>) -> FmtResult {
        for line in self.lines() {
            writeln!(f, "{}", style(line).dim())?;
        }
        Ok(())
    }
}
