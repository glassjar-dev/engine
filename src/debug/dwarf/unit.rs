use std::{collections::HashMap, num::NonZeroU64, path::PathBuf};

use crate::{
    debug::dwarf::{DerefContext, Die, Dwarf},
    types::CodeOffset,
    util::weak_error,
};

use super::R;
use gimli::{Reader, UnitHeader};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone)]
pub struct UnitProperties {
    /// The index of this unit among all units in the dwarf output
    pub index: usize,
    /// The index of the first file in this unit in the global locations list
    pub file_offset: usize,
}

/// Source language of a compilation unit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Language {
    Rust,
    C,
    Cpp,
    Unknown,
}

impl Language {
    fn from_dwarf(lang: gimli::DwLang) -> Self {
        match lang {
            gimli::DW_LANG_Rust => Self::Rust,
            gimli::DW_LANG_C89
            | gimli::DW_LANG_C
            | gimli::DW_LANG_C99
            | gimli::DW_LANG_C11
            | gimli::DW_LANG_C17 => Self::C,
            gimli::DW_LANG_C_plus_plus
            | gimli::DW_LANG_C_plus_plus_03
            | gimli::DW_LANG_C_plus_plus_11
            | gimli::DW_LANG_C_plus_plus_14
            | gimli::DW_LANG_C_plus_plus_17
            | gimli::DW_LANG_C_plus_plus_20 => Self::Cpp,
            other if other.0 == 0x003e => Self::C,   // C23
            other if other.0 == 0x003a => Self::Cpp, // C++23
            _ => Self::Unknown,
        }
    }
}

#[derive(Debug)]
pub struct Unit {
    /// Provides direct access to `gimli`
    unit: gimli::Unit<R>,
    properties: UnitProperties,
    files: Vec<PathBuf>,
    /// Map of parent DIE offsets within this unit.
    /// The keys are child offsets and the values are the parents of those children.
    pub(super) parents: HashMap<gimli::UnitOffset, gimli::UnitOffset>,
}

#[derive(PartialEq, Debug, Clone, Copy, Serialize, Deserialize)]
pub struct Location {
    /// Address within code segment
    pub address: CodeOffset,
    /// Index of the corresponding file.
    /// Use [Dwarf::file_at] to get the associated [PathBuf].
    pub file_index: usize,
    /// Line number within file (one-indexed)
    pub line: usize,
    /// Column number (one-indexed)
    pub column: usize,
}

impl std::ops::Deref for Unit {
    type Target = gimli::Unit<R>;

    fn deref(&self) -> &Self::Target {
        &self.unit
    }
}

impl Unit {
    pub fn clone(&self, dwarf: &gimli::Dwarf<R>) -> Self {
        let unit = {
            dwarf
                .unit(self.unit.header.clone())
                .expect("clone unit should not fail")
        };

        Self {
            unit,
            properties: self.properties.clone(),
            files: self.files.clone(),
            parents: self.parents.clone(),
        }
    }

    /// Gets the root DIE for this unit
    pub fn root<'a>(&'a self, dwarf: &'a Dwarf) -> Option<Die<'a>> {
        let mut entries = self.unit.entries();
        weak_error!(entries.next_entry())?;
        let die = weak_error!(entries.current().ok_or(gimli::Error::MissingUnitDie))?.clone();
        Some(Die::new(DerefContext::new(dwarf, self), die))
    }

    pub fn unit(&self) -> &gimli::Unit<R> {
        &self.unit
    }

    pub fn properties(&self) -> &UnitProperties {
        &self.properties
    }

    pub fn index(&self) -> usize {
        self.properties.index
    }

    /// Returns the source language of this compilation unit.
    pub fn language(&self) -> Language {
        let mut entries = self.unit.entries();
        let Some(true) = weak_error!(entries.next_entry()) else {
            return Language::Unknown;
        };
        let Some(die) = entries.current() else {
            return Language::Unknown;
        };
        let Some(attr) = die.attr(gimli::DW_AT_language) else {
            return Language::Unknown;
        };
        let gimli::AttributeValue::Language(lang) = attr.value() else {
            return Language::Unknown;
        };
        Language::from_dwarf(lang)
    }

    pub fn locations(&self) -> impl Iterator<Item = Location> {
        let Some(line_program) = self.unit.line_program.clone() else {
            return Vec::new().into_iter();
        };

        let mut rows = line_program.rows();
        weak_error!(parse_lines(self.properties.file_offset, &mut rows))
            .unwrap_or_default()
            .into_iter()
    }

    pub fn file_at(&self, index: usize) -> Option<&PathBuf> {
        self.files.get(index)
    }
}

pub struct UnitParser<'a> {
    dwarf: &'a gimli::Dwarf<R>,
    unit_index: usize,
    file_index: usize,
}

impl<'a> UnitParser<'a> {
    pub fn new(dwarf: &'a gimli::Dwarf<R>) -> Self {
        UnitParser {
            dwarf,
            unit_index: 0,
            file_index: 0,
        }
    }

    pub fn parse(&mut self, header: UnitHeader<R>) -> Option<Unit> {
        let unit = weak_error!(self.dwarf.unit(header))?;

        let mut files = vec![];
        if let Some(ref lp) = unit.line_program {
            let rows = lp.clone().rows();
            files = weak_error!(parse_files(self.dwarf, &unit, &rows))?;
        }

        let index = self.unit_index;
        self.unit_index += 1;

        let file_offset = self.file_index;
        self.file_index += files.len();

        let parents = weak_error!(parse_parents(&unit)).unwrap_or_default();

        Some(Unit {
            properties: UnitProperties { index, file_offset },
            unit,
            files,
            parents,
        })
    }
}

fn parse_parents(
    unit: &gimli::Unit<R>,
) -> gimli::Result<HashMap<gimli::UnitOffset, gimli::UnitOffset>> {
    let mut parents = HashMap::new();
    let mut stack = Vec::new();

    let mut entries = unit.entries();
    while entries.next_entry()? {
        let Some(die) = entries.current() else {
            continue;
        };
        if die.is_null() {
            continue;
        }

        let depth = entries.depth() as usize;
        if depth > 0 {
            parents.insert(die.offset, stack[depth - 1]);
        }

        if depth >= stack.len() {
            stack.push(die.offset);
        } else {
            stack[depth] = die.offset;
            stack.truncate(depth + 1);
        }
    }

    Ok(parents)
}

fn parse_lines(
    file_offset: usize,
    rows: &mut gimli::LineRows<R, gimli::IncompleteLineProgram<R>>,
) -> gimli::Result<Vec<Location>> {
    let mut lines = vec![];
    while let Some((_, line_row)) = rows.next_row()? {
        let column = match line_row.column() {
            gimli::ColumnType::LeftEdge => 1,
            gimli::ColumnType::Column(x) => x.get(),
        };

        if !line_row.is_stmt() {
            continue;
        }

        lines.push(Location {
            address: line_row.address().into(),
            file_index: file_offset + line_row.file_index() as usize,
            line: line_row.line().map(NonZeroU64::get).unwrap_or(0) as usize,
            column: column as usize,
        })
    }

    lines.shrink_to_fit();
    Ok(lines)
}

fn parse_files(
    dwarf: &gimli::Dwarf<R>,
    unit: &gimli::Unit<R>,
    rows: &gimli::LineRows<R, gimli::IncompleteLineProgram<R>>,
) -> gimli::Result<Vec<PathBuf>> {
    let mut files = vec![];
    let header = rows.header();
    match header.file(0) {
        Some(file) => files.push(render_file_path(unit, file, header, dwarf)?),
        None => files.push(PathBuf::default()),
    }
    let mut index = 1;
    while let Some(file) = header.file(index) {
        files.push(render_file_path(unit, file, header, dwarf)?);
        index += 1;
    }

    files.shrink_to_fit();
    Ok(files)
}

fn render_file_path(
    dw_unit: &gimli::Unit<R>,
    file: &gimli::FileEntry<R>,
    header: &gimli::LineProgramHeader<R>,
    sections: &gimli::Dwarf<R>,
) -> gimli::Result<PathBuf> {
    let mut path = if let Some(ref comp_dir) = dw_unit.comp_dir {
        PathBuf::from(comp_dir.to_string_lossy()?.as_ref())
    } else {
        PathBuf::new()
    };

    if file.directory_index() != 0
        && let Some(directory) = file.directory(header)
    {
        path.push(
            sections
                .attr_string(dw_unit, directory)?
                .to_string_lossy()?
                .as_ref(),
        );
    }

    path.push(
        sections
            .attr_string(dw_unit, file.path_name())?
            .to_string_lossy()?
            .as_ref(),
    );

    Ok(path)
}
