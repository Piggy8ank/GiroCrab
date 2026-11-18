import SwiftUI
import Charts
import GiroCrabCore

/// How far back a chart/list reaches. Kept as plain ISO date strings (the
/// core already works in those) rather than carrying `Date` through.
private enum HistoryRange: String, CaseIterable, Identifiable {
    case oneYear = "1 Jahr"
    case threeYears = "3 Jahre"
    case fiveYears = "5 Jahre"
    case all = "Alles"
    var id: String { rawValue }

    var years: Int {
        switch self {
        case .oneYear: 1
        case .threeYears: 3
        case .fiveYears: 5
        case .all: 20
        }
    }

    /// (from, to) as ISO `YYYY-MM-DD`, inclusive.
    func bounds() -> (from: String, to: String) {
        let to = Date()
        let from = Calendar.current.date(byAdding: .year, value: -years, to: to) ?? to
        return (DateText.iso(from), DateText.iso(to))
    }
}

/// Depot-level evaluations: value over time, allocation by position,
/// dividend payments. Opened from the depot's header row.
struct DepotAnalyticsSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let account: Account

    private enum Tab: String, CaseIterable, Identifiable {
        case performance = "Performance"
        case allocation = "Verteilung"
        case dividends = "Dividenden"
        var id: String { rawValue }
    }
    @State private var tab: Tab = .performance
    @State private var range: HistoryRange = .oneYear
    @State private var performance: [PortfolioValuePoint] = []
    @State private var allocation: [PositionAllocation] = []
    @State private var dividends: [DividendPayment] = []
    @State private var loading = false

    var body: some View {
        VStack(spacing: 0) {
            HStack {
                VStack(alignment: .leading, spacing: 2) {
                    Text("Aktien-Auswertung").font(.title3.weight(.semibold))
                    Text(account.name).font(.caption).foregroundStyle(Theme.secondary)
                }
                Spacer()
                Button("Fertig") { dismiss() }.keyboardShortcut(.defaultAction)
            }
            .padding()
            Picker("Ansicht", selection: $tab) {
                ForEach(Tab.allCases) { Text(LocalizedStringKey($0.rawValue)).tag($0) }
            }
            .pickerStyle(.segmented)
            .padding(.horizontal)
            .padding(.bottom, 8)
            if tab != .allocation {
                Picker("Zeitraum", selection: $range) {
                    ForEach(HistoryRange.allCases) { Text(LocalizedStringKey($0.rawValue)).tag($0) }
                }
                .pickerStyle(.segmented)
                .padding(.horizontal)
                .padding(.bottom, 8)
            }
            Divider()
            content.frame(maxWidth: .infinity, maxHeight: .infinity)
        }
        .frame(minWidth: 480, idealWidth: 560, minHeight: 420, idealHeight: 520)
        .task(id: "\(tab.rawValue)-\(range.rawValue)-\(account.id)") { await reload() }
    }

    @ViewBuilder
    private var content: some View {
        switch tab {
        case .performance:
            if loading {
                ProgressView().frame(maxWidth: .infinity, maxHeight: .infinity)
            } else if performance.count < 2 {
                ContentUnavailableView(
                    "Noch keine Kursverlaufsdaten",
                    systemImage: "chart.line.uptrend.xyaxis",
                    description: Text("Jeder Depot-Sync schreibt einen Tageskurs mit. Für eine Historie vor dem ersten Sync: Position öffnen und „Externe Kurshistorie laden“.")
                )
            } else {
                PortfolioValueChart(points: performance).padding()
            }
        case .allocation:
            if allocation.isEmpty {
                ContentUnavailableView("Keine Positionen mit Kurswert", systemImage: "chart.pie")
            } else {
                AllocationChart(rows: allocation).padding()
            }
        case .dividends:
            if dividends.isEmpty {
                ContentUnavailableView(
                    "Keine Dividenden gefunden",
                    systemImage: "banknote",
                    description: Text("Erkennt Gutschriften, deren Verwendungszweck/Buchungstext „Dividende“ oder „Ausschüttung“ enthält – keine exakte Zuordnung je Position.")
                )
            } else {
                DividendPaymentsList(rows: dividends).padding()
            }
        }
    }

    private func reload() async {
        loading = true
        defer { loading = false }
        let accID = account.id
        let (from, to) = range.bounds()
        switch tab {
        case .performance:
            performance = (try? await model.vaultOffMain {
                try $0.portfolioValueOverTime(accountId: accID, from: from, to: to)
            }) ?? []
        case .allocation:
            allocation = (try? await model.vaultOffMain { try $0.depotAllocation(accountId: accID) }) ?? []
        case .dividends:
            dividends = (try? await model.vaultOffMain {
                try $0.listDividendPayments(accountId: accID, from: from, to: to)
            }) ?? []
        }
    }
}

private struct PortfolioValueChart: View {
    let points: [PortfolioValuePoint]

    private struct P: Identifiable {
        let id: Int
        let date: Date
        let euros: Double
    }
    private var series: [P] {
        points.enumerated().compactMap { i, p in
            DateText.date(p.date).map { P(id: i, date: $0, euros: Double(p.valueCents) / 100) }
        }
    }

    var body: some View {
        Chart(series) { p in
            AreaMark(x: .value("Datum", p.date), y: .value("Wert", p.euros))
                .foregroundStyle(Theme.invest.opacity(0.12))
            LineMark(x: .value("Datum", p.date), y: .value("Wert", p.euros))
                .foregroundStyle(Theme.invest)
                .interpolationMethod(.monotone)
        }
        .chartYAxis {
            AxisMarks { value in
                AxisGridLine()
                AxisValueLabel {
                    if let d = value.as(Double.self) { Text(Money.format(Int64((d * 100).rounded()))) }
                }
            }
        }
        .frame(height: 240)
        .accessibilityLabel("Depotwert über Zeit")
    }
}

private struct AllocationChart: View {
    let rows: [PositionAllocation]

    private struct Row: Identifiable {
        let id: Int
        let allocation: PositionAllocation
    }
    private var indexed: [Row] {
        rows.enumerated().map { Row(id: $0.offset, allocation: $0.element) }
    }

    var body: some View {
        Chart(indexed) { item in
            let r = item.allocation
            BarMark(
                x: .value("Anteil", r.share),
                y: .value("Position", r.name.isEmpty ? (r.isin.isEmpty ? r.wkn : r.isin) : r.name)
            )
            .foregroundStyle(Theme.invest)
            .annotation(position: .trailing) {
                Text(r.share.formatted(.percent.precision(.fractionLength(1))))
                    .font(.caption2).foregroundStyle(Theme.secondary)
            }
        }
        .chartXAxis {
            AxisMarks { value in
                AxisGridLine()
                AxisValueLabel { if let d = value.as(Double.self) { Text(d.formatted(.percent)) } }
            }
        }
        .frame(height: max(CGFloat(rows.count) * 32, 80))
        .accessibilityLabel("Verteilung nach Position")
    }
}

private struct DividendPaymentsList: View {
    let rows: [DividendPayment]
    private var totalCents: Int64 { rows.reduce(0) { $0 + $1.amountCents } }

    var body: some View {
        VStack(alignment: .leading, spacing: 8) {
            Text("Gesamt im Zeitraum: \(Money.format(totalCents))")
                .font(.callout.weight(.semibold))
            List(rows.indices, id: \.self) { i in
                let r = rows[i]
                HStack {
                    VStack(alignment: .leading, spacing: 1) {
                        Text(r.counterparty).lineLimit(1)
                        Text(DateText.medium(r.bookingDate)).font(.caption).foregroundStyle(Theme.secondary)
                    }
                    Spacer()
                    Text(Money.format(r.amountCents, currency: r.currency)).monospacedDigit()
                        .foregroundStyle(Theme.inflow)
                }
            }
            .listStyle(.plain)
            .frame(minHeight: 160, maxHeight: .infinity)
        }
    }
}

/// One position's history, purchase marker and manual purchase entry.
/// Opened by tapping a position in `HoldingsCard`.
struct PositionDetailSheet: View {
    @Environment(AppModel.self) private var model
    @Environment(\.dismiss) private var dismiss
    let account: Account
    let holding: Holding

    @State private var history: [SecurityPricePoint] = []
    @State private var performance: PositionPerformance?
    @State private var hasPurchase = false
    @State private var purchaseDate = Date()
    @State private var priceText = ""
    @State private var note = ""
    @State private var backfilling = false
    @State private var saving = false
    @State private var tickerSymbol = ""

    var body: some View {
        ScrollView {
            VStack(alignment: .leading, spacing: 16) {
                header
                priceChartSection
                performanceSection
                purchaseForm
            }
            .padding(20)
        }
        .frame(minWidth: 440, idealWidth: 480, minHeight: 440, idealHeight: 560)
        .task(id: "\(holding.isin)-\(holding.wkn)") { await reload() }
        .toolbar {
            ToolbarItem(placement: .cancellationAction) {
                Button("Fertig") { dismiss() }.keyboardShortcut(.defaultAction)
            }
        }
    }

    private var header: some View {
        VStack(alignment: .leading, spacing: 2) {
            Text(holding.name.isEmpty ? displayID : holding.name).font(.title3.weight(.semibold))
            Text([displayID, "\(holding.quantity.formatted(.number.precision(.fractionLength(0...4)))) Stück"].joined(separator: " · "))
                .font(.caption).foregroundStyle(Theme.secondary)
        }
    }
    private var displayID: String { holding.isin.isEmpty ? holding.wkn : holding.isin }

    @ViewBuilder
    private var priceChartSection: some View {
        if history.count >= 2 {
            PositionPriceChart(history: history, purchasedAt: hasPurchase ? purchaseDate : nil)
        } else {
            ContentUnavailableView(
                "Noch keine Kurshistorie",
                systemImage: "chart.line.uptrend.xyaxis",
                description: Text("Entsteht automatisch mit jedem Sync, oder unten per externer Kurshistorie laden.")
            )
            .frame(height: 160)
        }
        VStack(alignment: .leading, spacing: 6) {
            HStack {
                TextField("Ticker-Symbol für Alpha Vantage, z. B. SAP.DEX", text: $tickerSymbol)
                    #if os(iOS)
                    .textInputAutocapitalization(.characters)
                    #endif
                    .frame(maxWidth: 260)
                Button {
                    Task { await backfill() }
                } label: {
                    if backfilling {
                        ProgressView().controlSize(.small)
                    } else {
                        Label("Externe Kurshistorie laden", systemImage: "arrow.down.circle")
                    }
                }
                .disabled(backfilling || holding.isin.isEmpty || tickerSymbol.trimmingCharacters(in: .whitespaces).isEmpty)
                Spacer()
            }
            Text("Benötigt einen kostenlosen Alpha-Vantage-API-Key in den Einstellungen; das Ticker-Symbol wird für diese ISIN gemerkt.")
                .font(.caption2).foregroundStyle(Theme.secondary)
        }
    }

    @ViewBuilder
    private var performanceSection: some View {
        if let p = performance, let gain = p.gainCents, let pct = p.gainPercent {
            HStack {
                Text("GuV seit Kauf").font(.callout.weight(.semibold))
                Spacer()
                Text("\(Money.format(gain)) (\(pct.formatted(.percent.precision(.fractionLength(1)))))")
                    .monospacedDigit()
                    .foregroundStyle(gain < 0 ? Theme.negative : Theme.inflow)
            }
            .padding(10)
            .background(RoundedRectangle(cornerRadius: 8).fill(Theme.neutralTile))
        }
    }

    private var purchaseForm: some View {
        Form {
            Section {
                Toggle("Kaufdatum erfasst", isOn: $hasPurchase)
                if hasPurchase {
                    DatePicker("Kaufdatum", selection: $purchaseDate, in: ...Date(), displayedComponents: .date)
                    TextField("Kaufpreis (pro Stück)", text: $priceText, prompt: Text("optional, z. B. \(Money.plain(holding.currentPriceCents))"))
                        #if os(iOS)
                        .keyboardType(.numbersAndPunctuation)
                        #endif
                    TextField("Notiz", text: $note)
                }
            } header: {
                Text("Kaufzeitpunkt (manuell)")
            } footer: {
                Text("Nicht alle Banken liefern den Kaufzeitpunkt per Sync. Hier eingetragen, bleibt er auch nach dem nächsten Sync erhalten.")
                    .font(.caption).foregroundStyle(Theme.secondary)
            }
            Section {
                Button("Speichern") { Task { await save() } }
                    .disabled(saving)
            }
        }
        .formStyle(.grouped)
        .frame(minHeight: 220)
    }

    private func reload() async {
        let isin = holding.isin
        history = (try? await model.vaultOffMain { try $0.securityPriceHistory(isin: isin) }) ?? []
        tickerSymbol = (try? await model.vaultOffMain { try $0.securityTicker(isin: isin) })?.flatMap { $0 } ?? ""
        let accID = account.id
        let wkn = holding.wkn
        performance = try? await model.vaultOffMain {
            try $0.positionPerformance(accountId: accID, isin: isin, wkn: wkn)
        }
        if let p = performance, !p.purchasedAt.isEmpty {
            hasPurchase = true
            purchaseDate = DateText.date(p.purchasedAt) ?? Date()
            priceText = p.purchasePriceCents.map(Money.plain) ?? ""
        } else {
            hasPurchase = false
        }
    }

    private func save() async {
        saving = true
        defer { saving = false }
        let accID = account.id
        let isin = holding.isin
        let wkn = holding.wkn
        let purchasedAt = hasPurchase ? DateText.iso(purchaseDate) : ""
        let priceCents = hasPurchase ? Money.parseCents(priceText) : nil
        let noteValue = note
        do {
            try await model.requireVaultOffMain {
                try $0.setHoldingPurchase(
                    accountId: accID, isin: isin, wkn: wkn,
                    purchasedAt: purchasedAt, purchasePriceCents: priceCents, note: noteValue
                )
            }
            await reload()
        } catch {
            model.showError(String(localized: "Speichern fehlgeschlagen"), describe(error))
        }
    }

    private func backfill() async {
        let symbol = tickerSymbol.trimmingCharacters(in: .whitespaces)
        guard !holding.isin.isEmpty, !symbol.isEmpty else { return }
        backfilling = true
        defer { backfilling = false }
        let isin = holding.isin
        let to = DateText.iso(Date())
        let from = DateText.iso(Calendar.current.date(byAdding: .year, value: -5, to: Date()) ?? Date())
        do {
            try await model.requireVaultOffMain { try $0.setSecurityTicker(isin: isin, symbol: symbol) }
            guard let vault = model.vault else { return }
            _ = try await vault.backfillSecurityPriceHistory(isin: isin, from: from, to: to)
            await reload()
        } catch {
            model.showError(String(localized: "Kurshistorie konnte nicht geladen werden"), describe(error))
        }
    }
}

private struct PositionPriceChart: View {
    let history: [SecurityPricePoint]
    let purchasedAt: Date?

    private struct P: Identifiable {
        let id: Int
        let date: Date
        let euros: Double
    }
    private var series: [P] {
        history.enumerated().compactMap { i, p in
            DateText.date(p.priceDate).map { P(id: i, date: $0, euros: Double(p.closePriceCents) / 100) }
        }
    }

    var body: some View {
        Chart(series) { p in
            LineMark(x: .value("Datum", p.date), y: .value("Kurs", p.euros))
                .foregroundStyle(Theme.invest)
                .interpolationMethod(.monotone)
            if let purchasedAt {
                RuleMark(x: .value("Kauf", purchasedAt))
                    .foregroundStyle(Theme.warning)
                    .lineStyle(StrokeStyle(lineWidth: 1, dash: [4, 3]))
                    .annotation(position: .top, alignment: .leading) {
                        Text("Kauf").font(.caption2).foregroundStyle(Theme.warning)
                    }
            }
        }
        .chartYAxis {
            AxisMarks { value in
                AxisGridLine()
                AxisValueLabel {
                    if let d = value.as(Double.self) { Text(Money.format(Int64((d * 100).rounded()))) }
                }
            }
        }
        .frame(height: 200)
        .accessibilityLabel("Kursverlauf mit Kaufmarkierung")
    }
}
