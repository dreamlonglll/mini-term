export const time = {
  zh: {
    justNow: "刚刚",
    minutesAgo: "{n} 分钟前",
    hoursAgo: "{n} 小时前",
    daysAgo: "{n} 天前",
    // 日期选择浮层（crates/mt-app/src/date_picker.rs）。月份标题走
    // `YYYY-MM` 纯数字，与日期输入框的 `YYYY-MM-DD` 同源，中英通吃不进字典。
    prevMonth: "上个月",
    nextMonth: "下个月",
    weekday: {
      sun: "日",
      mon: "一",
      tue: "二",
      wed: "三",
      thu: "四",
      fri: "五",
      sat: "六",
    },
  },
  en: {
    justNow: "just now",
    minutesAgo: "{n} min ago",
    hoursAgo: "{n} hr ago",
    daysAgo: "{n} days ago",
    prevMonth: "Previous month",
    nextMonth: "Next month",
    weekday: {
      sun: "Su",
      mon: "Mo",
      tue: "Tu",
      wed: "We",
      thu: "Th",
      fri: "Fr",
      sat: "Sa",
    },
  },
} as const;
