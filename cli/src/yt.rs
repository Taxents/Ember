//! `music-cli yt …`: calls the InnerTube client and prints what the parsers made of it.

use music_core::yt::Client;
use music_core::yt::model::*;
use music_core::{Error, Result};

pub fn run(args: &[String], json: bool) -> Result<()> {
    let client = Client::from_disk();
    let a = |i: usize| args.get(i).map(String::as_str).ok_or_else(usage);
    let rest = || args[1..].join(" ");
    let out: Out = match a(0)? {
        "home" => client.home()?.into(),
        "explore" => client.explore()?.into(),
        "new" => client.new_releases()?.into(),
        "charts" => client.charts()?.into(),
        "moods" => client.moods()?.into(),
        "browse" => client
            .feed(&BrowseRef {
                browse_id: a(1)?.into(),
                params: args.get(2).cloned(),
            })?
            .into(),
        "search" => Out::Search(client.search(&rest(), None)?),
        "songs" | "videos" | "albums" | "artists" | "playlists" => {
            let f = match a(0)? {
                "songs" => SearchFilter::Songs,
                "videos" => SearchFilter::Videos,
                "albums" => SearchFilter::Albums,
                "artists" => SearchFilter::Artists,
                _ => SearchFilter::Playlists,
            };
            let first = client.search(&rest(), Some(f))?;
            let n = first.items.len();
            let more = match &first.continuation {
                Some(c) => client.search_more(&rest(), Some(f), c)?.items.len(),
                None => 0,
            };
            eprintln!("({n} results, next page {more})");
            Out::Search(first)
        }
        "suggest" => {
            let (words, items) = client.suggestions(&rest())?;
            for w in words {
                println!("  {w}");
            }
            items.iter().for_each(print_item);
            return Ok(());
        }
        "album" => Out::Album(client.album(a(1)?)?),
        "artist" => Out::Artist(client.artist(a(1)?)?),
        "playlist" => Out::Playlist(client.playlist_all(a(1)?, 1000)?),
        "radio" => {
            let w = client.radio(a(1)?)?;
            let more = match &w.continuation {
                Some(c) => {
                    let pl = format!("RDAMVM{}", a(1)?);
                    client
                        .watch_next_more(Some(a(1)?), Some(&pl), None, c)?
                        .tracks
                        .len()
                }
                None => 0,
            };
            eprintln!("(continuation adds {more})");
            Out::Watch(w)
        }
        "library" => match a(1)? {
            "playlists" => client.feed_all(client.library_playlists()?, 5000)?.into(),
            "songs" => client.feed_all(client.library_songs()?, 5000)?.into(),
            "albums" => client.feed_all(client.library_albums()?, 5000)?.into(),
            "artists" => client.feed_all(client.library_artists()?, 5000)?.into(),
            "subscriptions" => client.feed_all(client.subscriptions()?, 5000)?.into(),
            "history" => client.history()?.into(),
            "liked" => Out::Playlist(client.liked_songs(5000)?),
            _ => return Err(usage()),
        },
        "whoami" => {
            let acc = client.account()?;
            println!("{} {}", acc.name, acc.handle.unwrap_or_default());
            return Ok(());
        }
        "like" | "dislike" | "unlike" => {
            let l = match a(0)? {
                "like" => Like::Like,
                "dislike" => Like::Dislike,
                _ => Like::Indifferent,
            };
            client.rate(a(1)?, l)?;
            println!("ok");
            return Ok(());
        }
        "save" | "unsave" => {
            client.save_playlist(a(1)?, a(0)? == "save")?;
            println!("ok");
            return Ok(());
        }
        "create" => {
            let ids: Vec<String> = args[2..].to_vec();
            println!("{}", client.create_playlist(a(1)?, "", &ids)?);
            return Ok(());
        }
        "add" => {
            client.add_to_playlist(a(1)?, &args[2..])?;
            println!("ok");
            return Ok(());
        }
        "remove" => {
            let p = client.playlist_all(a(1)?, 5000)?;
            let ts: Vec<Track> = p
                .tracks
                .into_iter()
                .filter(|t| args[2..].contains(&t.video_id))
                .collect();
            client.remove_from_playlist(a(1)?, &ts)?;
            println!("removed {}", ts.len());
            return Ok(());
        }
        "rename" => {
            client.rename_playlist(a(1)?, &args[2..].join(" "))?;
            println!("ok");
            return Ok(());
        }
        "delete" => {
            client.delete_playlist(a(1)?)?;
            println!("ok");
            return Ok(());
        }
        "subscribe" | "unsubscribe" => {
            client.subscribe(a(1)?, a(0)? == "subscribe")?;
            println!("ok");
            return Ok(());
        }
        "report" => {
            client.report_play(a(1)?)?;
            println!("ok");
            return Ok(());
        }
        _ => return Err(usage()),
    };
    if json {
        println!("{}", out.json());
    } else {
        out.print();
    }
    Ok(())
}

fn usage() -> Error {
    Error::Unavailable(
        "yt home|explore|new|charts|moods | browse <id> [params] | search|songs|videos|albums|artists|playlists <q> \
         | suggest <q> | album|artist|playlist|radio <id> | library playlists|songs|albums|artists|subscriptions|history|liked \
         | whoami | like|dislike|unlike <vid> | save|unsave <playlistId> | create <title> [vid…] | add <pl> <vid…> \
         | remove <pl> <vid…> | rename <pl> <title> | delete <pl> | report <vid>   [--json]"
            .into(),
    )
}

enum Out {
    Feed(Feed),
    Search(SearchResults),
    Album(AlbumPage),
    Artist(ArtistPage),
    Playlist(PlaylistPage),
    Watch(WatchNext),
}

impl From<Feed> for Out {
    fn from(f: Feed) -> Self {
        Out::Feed(f)
    }
}

impl Out {
    fn json(&self) -> String {
        match self {
            Out::Feed(x) => serde_json::to_string_pretty(x),
            Out::Search(x) => serde_json::to_string_pretty(x),
            Out::Album(x) => serde_json::to_string_pretty(x),
            Out::Artist(x) => serde_json::to_string_pretty(x),
            Out::Playlist(x) => serde_json::to_string_pretty(x),
            Out::Watch(x) => serde_json::to_string_pretty(x),
        }
        .expect("json")
    }

    fn print(&self) {
        match self {
            Out::Feed(f) => {
                if let Some(t) = &f.title {
                    println!("# {t}");
                }
                f.shelves.iter().for_each(print_shelf);
                println!("continuation: {}", f.continuation.is_some());
            }
            Out::Search(r) => {
                if let Some(t) = &r.top {
                    print!("top: ");
                    print_item(t);
                    r.top_extra.iter().for_each(|i| {
                        print!("   ");
                        print_item(i)
                    });
                }
                r.items.iter().for_each(print_item);
                println!("continuation: {}", r.continuation.is_some());
            }
            Out::Album(a) => {
                println!(
                    "# {} — {} ({:?}, {:?}) {} | playlist {:?}{}",
                    a.title,
                    names(&a.artists),
                    a.kind,
                    a.year,
                    a.summary,
                    a.playlist_id,
                    if a.explicit { " [E]" } else { "" }
                );
                if let Some(d) = &a.description {
                    println!("  {}", clip(d, 120));
                }
                a.tracks.iter().for_each(print_track);
                a.shelves.iter().for_each(print_shelf);
            }
            Out::Artist(a) => {
                println!(
                    "# {} | {:?} | subscribed {:?} | channel {:?}",
                    a.name, a.audience, a.subscribed, a.channel_id
                );
                println!("  shuffle {:?}\n  radio {:?}", a.shuffle, a.radio);
                if let Some(d) = &a.description {
                    println!("  {}", clip(d, 120));
                }
                println!(
                    "  {} header images, widest {}",
                    a.thumbs.len(),
                    a.thumbs.iter().map(|t| t.width).max().unwrap_or(0)
                );
                a.shelves.iter().for_each(print_shelf);
            }
            Out::Playlist(p) => {
                println!(
                    "# {} — {:?} | {} | {} | owned {} | {} tracks",
                    p.title,
                    p.author.as_ref().map(|a| &a.name),
                    p.subtitle,
                    p.summary,
                    p.owned,
                    p.tracks.len()
                );
                p.tracks.iter().take(30).for_each(print_track);
                if p.tracks.len() > 30 {
                    println!("  …");
                }
                println!("continuation: {}", p.continuation.is_some());
            }
            Out::Watch(w) => {
                println!(
                    "# {:?} | lyrics {:?} | {} tracks",
                    w.playlist_id,
                    w.lyrics_id,
                    w.tracks.len()
                );
                w.tracks.iter().for_each(print_track);
                println!("continuation: {}", w.continuation.is_some());
            }
        }
    }
}

fn print_shelf(s: &Shelf) {
    println!(
        "\n## {} ({} items){}",
        s.title,
        s.items.len(),
        s.more
            .as_ref()
            .map(|m| format!(" → {}", m.browse_id))
            .unwrap_or_default()
    );
    s.items.iter().take(8).for_each(|i| {
        print!("  ");
        print_item(i)
    });
    if s.items.len() > 8 {
        println!("  …");
    }
}

fn print_item(i: &Item) {
    match i {
        Item::Track(t) => print_track(t),
        Item::Album(a) => println!(
            "  [album {:?}] {} — {} ({:?}) {}{}",
            a.kind,
            a.title,
            names(&a.artists),
            a.year,
            a.id,
            if a.explicit { " [E]" } else { "" }
        ),
        Item::Artist(a) => println!("  [artist] {} | {} {}", a.name, a.subtitle, a.id),
        Item::Playlist(p) => println!("  [playlist] {} | {} {}", p.title, p.subtitle, p.id),
        Item::Link(l) => println!(
            "  [link] {} {} {:?} {:?}",
            l.title,
            l.browse_id,
            l.params.as_deref().map(|p| clip(p, 12)),
            l.color
        ),
    }
}

fn print_track(t: &Track) {
    println!(
        "  {}{} {} — {}{} [{}] {}{}{}{}",
        if t.kind == TrackKind::Video {
            "🎬"
        } else {
            "♪"
        },
        if t.unavailable { "✗" } else { "" },
        t.title,
        names(&t.artists),
        t.album
            .as_ref()
            .map(|a| format!(" · {}", a.name))
            .unwrap_or_default(),
        t.duration_ms
            .map(|d| format!("{}:{:02}", d / 60000, d / 1000 % 60))
            .unwrap_or("?".into()),
        t.video_id,
        if t.explicit { " [E]" } else { "" },
        t.like.map(|l| format!(" {l:?}")).unwrap_or_default(),
        if t.thumbs.is_empty() { " (no art)" } else { "" },
    );
}

fn names(n: &[Named]) -> String {
    n.iter()
        .map(|a| {
            if a.id.is_some() {
                a.name.clone()
            } else {
                format!("{}*", a.name)
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

fn clip(s: &str, n: usize) -> String {
    let s = s.replace('\n', " ");
    if s.chars().count() > n {
        format!("{}…", s.chars().take(n).collect::<String>())
    } else {
        s
    }
}
