//! The library's Albums, sorted and grouped as picked: the library's own order (recently
//! added), by artist, title, year or genre. The whole library is fetched, so the order covers
//! all of it; genres arrive later ([`crate::genres`]) and regroup the page as they do.

use crate::*;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AlbumSort {
    /// The library's own order.
    #[default]
    Added,
    /// Grouped by artist, each one's newest first.
    Artist,
    Title,
    /// Grouped by decade.
    Newest,
    Oldest,
    /// Grouped by genre.
    Genre,
}

/// The choices, in the order the page shows them.
pub const SORTS: [(AlbumSort, &str); 6] = [
    (AlbumSort::Added, "Recently added"),
    (AlbumSort::Artist, "Artist"),
    (AlbumSort::Title, "Title"),
    (AlbumSort::Newest, "Newest"),
    (AlbumSort::Oldest, "Oldest"),
    (AlbumSort::Genre, "Genre"),
];

/// Where albums without a genre go.
pub const OTHER: &str = "Other";
/// Where albums go while their genre is still being looked up.
pub const LOOKING: &str = "Looking up…";

/// Albums in `sort`'s order, in groups under their headings (one untitled group when the sort
/// doesn't group). `genre` is what's known of an album's: `None` not looked up yet,
/// `Some(None)` none found.
pub fn arrange(
    items: &[Item],
    sort: AlbumSort,
    genre: impl Fn(&str) -> Option<Option<String>>,
) -> Vec<(Option<String>, Vec<Item>)> {
    let mut v: Vec<&Item> = items.iter().collect();
    let artist = |i: &Item| {
        sort_key(
            &album(i)
                .map(|a| pages::names(&a.artists))
                .unwrap_or_default(),
        )
    };
    let title = |i: &Item| sort_key(&album(i).map(|a| a.title.clone()).unwrap_or_default());
    let year = |i: &Item| album(i).and_then(|a| a.year);
    let grouped = |v: Vec<&Item>, head: &dyn Fn(&Item) -> String| {
        let mut groups: Vec<(Option<String>, Vec<Item>)> = Vec::new();
        for i in v {
            let h = head(i);
            match groups.last_mut() {
                Some((Some(last), items)) if *last == h => items.push(i.clone()),
                _ => groups.push((Some(h), vec![i.clone()])),
            }
        }
        groups
    };
    match sort {
        AlbumSort::Added => vec![(None, items.to_vec())],
        AlbumSort::Title => {
            v.sort_by_cached_key(|i| (title(i), artist(i)));
            vec![(None, v.into_iter().cloned().collect())]
        }
        AlbumSort::Artist => {
            // Each artist's newest first; the heading is the artist as credited.
            v.sort_by_cached_key(|i| {
                (artist(i), std::cmp::Reverse(year(i).unwrap_or(0)), title(i))
            });
            grouped(v, &|i| {
                album(i)
                    .and_then(|a| a.artists.first())
                    .map(|a| a.name.clone())
                    .filter(|n| !n.is_empty())
                    .unwrap_or_else(|| "Unknown artist".into())
            })
        }
        AlbumSort::Newest | AlbumSort::Oldest => {
            let newest = sort == AlbumSort::Newest;
            // Undated ones last either way.
            v.sort_by_cached_key(|i| {
                let y = year(i).map(|y| if newest { -(y as i64) } else { y as i64 });
                (y.is_none(), y, artist(i), title(i))
            });
            grouped(v, &|i| match year(i) {
                Some(y) => format!("{}s", y / 10 * 10),
                None => "Year unknown".into(),
            })
        }
        AlbumSort::Genre => {
            let head = |i: &Item| match album(i).map(|a| genre(&a.id)) {
                Some(Some(Some(g))) => g,
                Some(None) => LOOKING.to_string(),
                _ => OTHER.to_string(),
            };
            // Genres A–Z, then Other, then those still being looked up.
            v.sort_by_cached_key(|i| {
                let h = head(i);
                let rank = match h.as_str() {
                    OTHER => 1,
                    LOOKING => 2,
                    _ => 0,
                };
                (
                    rank,
                    h.to_lowercase(),
                    artist(i),
                    std::cmp::Reverse(year(i).unwrap_or(0)),
                    title(i),
                )
            });
            grouped(v, &head)
        }
    }
}

fn album(i: &Item) -> Option<&AlbumCard> {
    match i {
        Item::Album(a) => Some(a),
        _ => None,
    }
}

/// How a name sorts: without case, accents' marks, a leading "The " or leading symbols
/// ("$uicideboy$" with the U's).
pub fn sort_key(s: &str) -> String {
    let s = s.to_lowercase();
    let s = s.trim_start_matches(|c: char| !c.is_alphanumeric());
    let s = s.strip_prefix("the ").unwrap_or(s);
    s.chars()
        .map(|c| match c {
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' => 'a',
            'ç' => 'c',
            'è' | 'é' | 'ê' | 'ë' => 'e',
            'ì' | 'í' | 'î' | 'ï' => 'i',
            'ñ' => 'n',
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' => 'o',
            'ù' | 'ú' | 'û' | 'ü' => 'u',
            'ý' | 'ÿ' => 'y',
            c => c,
        })
        .collect()
}

impl App {
    /// The Albums page's albums arrived (or the sort changed, or genres came): laid out again
    /// in the picked order.
    pub(crate) fn show_albums(&mut self, items: Vec<Item>) {
        let sort = self.settings.album_sort;
        if sort == AlbumSort::Genre {
            let cards = items
                .iter()
                .filter_map(|i| album(i).cloned())
                .collect::<Vec<_>>();
            genres::look_up(cards, |found| {
                with(|a| a.genres_found(found));
            });
        }
        let groups = arrange(&items, sort, genres::known);
        self.albums = items;
        self.grid.clear();
        self.grid_heads.clear();
        for (head, items) in groups {
            if let Some(h) = head {
                self.grid_heads.push((self.grid.len(), h));
            }
            self.grid.extend(items);
        }
        // Laid out afresh from the grid's first row.
        self.grid_laid = (0, 0);
        self.layout_grid();
    }

    pub(crate) fn album_sort_picked(&mut self, i: usize) {
        let Some(&(sort, _)) = SORTS.get(i) else {
            return;
        };
        self.ui().global::<Ui>().set_album_sort(i as i32);
        if sort == self.settings.album_sort {
            return;
        }
        self.settings.album_sort = sort;
        self.settings.save();
        if self.on_albums() {
            let items = std::mem::take(&mut self.albums);
            self.show_albums(items);
            self.page_rows.scrolled(0.0);
            let ui = self.ui();
            let g = ui.global::<Ui>();
            g.set_page_scroll(0.0);
            g.set_scroll_serial(g.get_scroll_serial().wrapping_add(1));
        }
    }

    /// Some genres were found: the Albums page regroups, if it's showing them.
    fn genres_found(&mut self, found: Vec<(String, Option<String>)>) {
        if found.is_empty() || !self.on_albums() || self.settings.album_sort != AlbumSort::Genre {
            return;
        }
        let keep = self.page_rows.scroll();
        let items = std::mem::take(&mut self.albums);
        self.show_albums(items);
        self.page_rows.scrolled(keep);
    }

    pub(crate) fn on_albums(&self) -> bool {
        self.history.get(self.at).map(|r| r.kind) == Some(PageKind::Albums)
            && !self.albums.is_empty()
    }

    pub(crate) fn album_sort_index(&self) -> i32 {
        SORTS
            .iter()
            .position(|(s, _)| *s == self.settings.album_sort)
            .unwrap_or(0) as i32
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn album(id: &str, title: &str, artist: &str, year: Option<u32>) -> Item {
        Item::Album(AlbumCard {
            id: id.into(),
            title: title.into(),
            artists: vec![Named {
                name: artist.into(),
                id: None,
            }],
            year,
            ..Default::default()
        })
    }

    fn library() -> Vec<Item> {
        vec![
            album("1", "Blonde", "Frank Ocean", Some(2016)),
            album("2", "OK Computer", "Radiohead", Some(1997)),
            album("3", "Channel Orange", "Frank Ocean", Some(2012)),
            album("4", "Abbey Road", "The Beatles", Some(1969)),
            album("5", "Untitled", "Ämbient", None),
            album("6", "Kid A", "Radiohead", Some(2000)),
        ]
    }

    fn ids(items: &[Item]) -> Vec<String> {
        items
            .iter()
            .map(|i| pages::item_id(i).to_string())
            .collect()
    }

    fn shape(groups: &[(Option<String>, Vec<Item>)]) -> Vec<(String, Vec<String>)> {
        groups
            .iter()
            .map(|(h, items)| (h.clone().unwrap_or_default(), ids(items)))
            .collect()
    }

    fn none(_: &str) -> Option<Option<String>> {
        None
    }

    #[test]
    fn recently_added_keeps_the_library_order() {
        let g = arrange(&library(), AlbumSort::Added, none);
        assert_eq!(shape(&g), [("".into(), ids(&library()))]);
    }

    #[test]
    fn titles_sort_without_case_or_the() {
        let g = arrange(&library(), AlbumSort::Title, none);
        assert_eq!(shape(&g)[0].1, ["4", "1", "3", "6", "2", "5"]);
    }

    #[test]
    fn artists_group_newest_first() {
        let g = arrange(&library(), AlbumSort::Artist, none);
        let want = [
            ("Ämbient", vec!["5"]),
            ("The Beatles", vec!["4"]),
            ("Frank Ocean", vec!["1", "3"]),
            ("Radiohead", vec!["6", "2"]),
        ];
        let want: Vec<(String, Vec<String>)> = want
            .iter()
            .map(|(h, v)| (h.to_string(), v.iter().map(|s| s.to_string()).collect()))
            .collect();
        assert_eq!(shape(&g), want);
    }

    #[test]
    fn years_group_by_decade_undated_last() {
        let g = arrange(&library(), AlbumSort::Newest, none);
        let heads: Vec<String> = shape(&g).into_iter().map(|(h, _)| h).collect();
        assert_eq!(heads, ["2010s", "2000s", "1990s", "1960s", "Year unknown"]);
        assert_eq!(shape(&g)[0].1, ["1", "3"]);
        let g = arrange(&library(), AlbumSort::Oldest, none);
        let heads: Vec<String> = shape(&g).into_iter().map(|(h, _)| h).collect();
        assert_eq!(heads, ["1960s", "1990s", "2000s", "2010s", "Year unknown"]);
        assert_eq!(shape(&g)[3].1, ["3", "1"]);
    }

    #[test]
    fn genres_group_with_other_and_pending_last() {
        let genre = |id: &str| match id {
            "1" | "3" => Some(Some("R&B".to_string())),
            "2" | "6" | "4" => Some(Some("Rock".to_string())),
            "5" => Some(None),
            _ => None,
        };
        let mut lib = library();
        lib.push(album("7", "New", "Someone", Some(2024)));
        let g = arrange(&lib, AlbumSort::Genre, genre);
        let want = [
            ("R&B", vec!["1", "3"]),
            ("Rock", vec!["4", "6", "2"]),
            (OTHER, vec!["5"]),
            (LOOKING, vec!["7"]),
        ];
        let want: Vec<(String, Vec<String>)> = want
            .iter()
            .map(|(h, v)| (h.to_string(), v.iter().map(|s| s.to_string()).collect()))
            .collect();
        assert_eq!(shape(&g), want);
    }

    #[test]
    fn sort_keys() {
        assert_eq!(sort_key("The Beatles"), "beatles");
        assert_eq!(sort_key("Théâtre"), "theatre");
        assert_eq!(sort_key("  Ämbient"), "ambient");
        assert_eq!(sort_key("$uicideboy$"), "uicideboy$");
        assert_eq!(sort_key("(What's the Story)"), "what's the story)");
    }
}
